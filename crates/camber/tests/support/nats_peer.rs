//! A scripted Core NATS wire peer for component and acceptance rows.
//!
//! It speaks the text protocol the SDK needs: `INFO` on accept, `PONG` for
//! each `PING`, and a parsed log of `CONNECT`, `PUB`, `HPUB`, `SUB`, and
//! `UNSUB`. A row scripts the transport failures a real server cannot produce
//! on demand: it can stop reading, never answer the handshake, answer it with
//! an authorization error, or drop every connection and refuse the next ones.
//!
//! The peer never answers a publication by itself. A row reads each complete
//! [`Publication`] and decides if, when, and in which order a [`Reply`] goes
//! back; the peer routes it to every live subscription that matches its
//! subject, one member per queue group name, as a server would. Withheld
//! replies stall nothing: the peer keeps reading and answering `PING`
//! meanwhile.
//!
//! The peer owns its listener, its acceptor thread, and one thread per
//! accepted connection. [`NatsPeer::finish`] stops and joins them all within a
//! bound; `Drop` is the fallback for an unwinding row. Every wait it offers is a
//! bounded wait for a fact the peer itself recorded, never a sleep.

#[path = "nats_wire.rs"]
pub mod wire;

use crate::integration_rows::{ROW_BOUND, Row, bounded, expect_pending, refusal};
use crate::scripted_peer::{
    PEER_POLL as POLL, PEER_UNWINDING, PeerPlan, PeerState, PeerThreads, SharedState, Spawned,
    read_timed_out, shut_down_all, spawn_tracked,
};
use camber::mq::nats::Connection;
use camber::{IntegrationFailure, RuntimeError};
use std::future::Future;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use wire::{ADVERTISED_MAX_PAYLOAD, Command, Frame, Publication, Reply, Subscription};

/// How the peer answers what it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    /// Answer the handshake and every ping.
    Serve,
    /// Stop reading the socket; the connection stays open. The peer still
    /// records that bytes wait unread.
    Stall,
    /// Accept new connections but never send `INFO`; they stay open.
    Silent,
    /// Answer `CONNECT` with an authorization violation.
    DenyAuthorization,
    /// Drop every connection and close new ones on accept.
    Refuse,
}

/// What the peer has read, in order.
#[derive(Clone, Debug, Default)]
pub struct PeerLog {
    /// Connections accepted, refused ones included.
    pub accepted: usize,
    /// `CONNECT` commands read.
    pub connects: usize,
    /// `PING` commands read.
    pub pings: usize,
    /// Replies to server PINGs, after preceding deliveries were processed.
    pub pongs: usize,
    /// `PUB` and `HPUB` commands read: subject and payload length.
    pub published: Vec<(Box<str>, usize)>,
    /// Every complete `PUB` and `HPUB`, in full, in the order read.
    pub publications: Vec<Publication>,
    /// `SUB` commands read: subject, queue group, and subscription ID.
    pub subscribed: Vec<Subscription>,
    /// `UNSUB` commands read, by subscription ID.
    pub unsubscribed: Vec<Box<str>>,
    /// Connections the client closed: their reads reached end of stream or a
    /// reset the peer did not cause.
    pub closed: usize,
    /// Whether bytes ever waited unread on a stalled connection.
    pub unread: bool,
    /// Bytes read from every connection; each complete command and refused
    /// frame among them is recorded before this count grows.
    pub received: usize,
    /// Frames refused as malformed, by reason; each ended its connection.
    pub malformed: Vec<Box<str>>,
}

/// The state the peer's threads and the row share.
pub struct State {
    log: PeerLog,
    script: Script,
    stopping: bool,
    /// The connections still open, newest last.
    links: Vec<Link>,
    /// The identity the next connection takes.
    next_link: usize,
}

impl State {
    /// Shut down every open connection.
    fn cut(&mut self) {
        shut_down_all(self.links.drain(..).map(|link| link.writer));
    }

    /// Forget connection `id` once it ended, unless a cut already dropped it.
    fn close(&mut self, id: usize) {
        self.links.retain(|link| link.id != id);
    }

    /// The open connection `id`, unless it ended or a cut dropped it.
    fn link(&mut self, id: usize) -> Option<&mut Link> {
        self.links.iter_mut().find(|link| link.id == id)
    }

    /// The newest open connection.
    fn newest(&mut self) -> Result<&mut Link, String> {
        self.links
            .last_mut()
            .ok_or_else(|| "no open connection".to_owned())
    }
}

/// One open connection: its writer and its live subscriptions.
struct Link {
    id: usize,
    writer: TcpStream,
    routes: Vec<Route>,
}

impl Link {
    /// Write `frames` in one write.
    fn write(&mut self, frames: &[u8]) -> Result<(), String> {
        self.writer
            .write_all(frames)
            .map_err(|error| format!("write {} bytes: {error}", frames.len()))
    }

    /// Route `reply` to every live subscription matching `subject`, one
    /// member per queue group name, and retire routes that reached their
    /// maximum. A delivery is recorded only once its frame was written.
    fn route(&mut self, subject: &str, reply: &Reply<'_>) -> Result<Box<[Box<str>]>, String> {
        let chosen = self.chosen(subject);
        if !chosen.contains(&true) {
            return Err(format!("no live subscription matches {subject}"));
        }
        let mut frames = Vec::new();
        for (route, _) in self.routes.iter().zip(&chosen).filter(|(_, take)| **take) {
            reply.encode(subject, &route.sid, &mut frames);
        }
        self.write(&frames)?;
        let sids = self
            .routes
            .iter_mut()
            .zip(&chosen)
            .filter(|(_, take)| **take)
            .map(|(route, _)| {
                route.delivered += 1;
                route.sid.clone()
            })
            .collect();
        self.routes.retain(Route::live);
        Ok(sids)
    }

    /// Which routes take a reply on `subject`, in route order: every match
    /// outside a queue group, and the first match of each queue group name.
    ///
    /// A server merges same-named queue subscriptions across every matching
    /// subject into one group and picks one member; the peer picks the first.
    fn chosen(&self, subject: &str) -> Box<[bool]> {
        let mut groups: Vec<&str> = Vec::new();
        self.routes
            .iter()
            .map(|route| {
                if !wire::subject_matches(&route.subject, subject) {
                    return false;
                }
                match route.queue.as_deref() {
                    Some(group) if groups.contains(&group) => false,
                    Some(group) => {
                        groups.push(group);
                        true
                    }
                    None => true,
                }
            })
            .collect()
    }
}

/// One live subscription a connection can be replied on.
struct Route {
    subject: Box<str>,
    queue: Option<Box<str>>,
    sid: Box<str>,
    delivered: usize,
    /// The deliveries after which an `UNSUB` with a maximum ends it.
    max: Option<usize>,
}

impl Route {
    /// Whether the route has deliveries left.
    fn live(&self) -> bool {
        self.max.is_none_or(|max| self.delivered < max)
    }
}

impl PeerState for State {
    type Log = PeerLog;

    fn log(&self) -> &PeerLog {
        &self.log
    }

    fn accepted(&self) -> usize {
        self.log.accepted
    }

    fn stopping(&self) -> bool {
        self.stopping
    }

    fn stop(&mut self) {
        self.stopping = true;
        self.cut();
    }
}

type Shared = SharedState<State>;

/// Scripts and reads one NATS peer.
pub type PeerControl = crate::scripted_peer::PeerControl<State>;

/// A scripted NATS peer on an ephemeral loopback port.
///
/// The one owner of the peer's threads. A row hands [`PeerControl`] clones to
/// the code it runs and keeps the peer itself for [`Self::finish`].
pub struct NatsPeer {
    address: SocketAddr,
    threads: PeerThreads<PeerControl>,
    control: PeerControl,
}

impl NatsPeer {
    /// Bind and start serving with `Script::Serve`.
    ///
    /// # Panics
    ///
    /// When the loopback listener cannot bind: the row cannot run without it.
    #[must_use]
    pub fn start() -> Self {
        let control = PeerControl::new(State {
            log: PeerLog::default(),
            script: Script::Serve,
            stopping: false,
            links: Vec::new(),
            next_link: 0,
        });
        let admitting = Arc::clone(control.shared());
        let (threads, address) = PeerThreads::start(PeerPlan {
            poll: POLL,
            what: "scripted NATS peer threads",
            unwinding: PEER_UNWINDING,
            control: control.clone(),
            admit: move |stream, spawned: &Spawned| admit(stream, &admitting, spawned),
        })
        .expect("bind the scripted NATS peer");
        Self {
            address,
            threads,
            control,
        }
    }

    /// The loopback address the peer listens on.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// The URL an SDK client connects to.
    #[must_use]
    pub fn url(&self) -> String {
        format!("nats://{}", self.address)
    }

    /// Script and read this peer.
    #[must_use]
    pub fn control(&self) -> PeerControl {
        self.control.clone()
    }

    /// Stop the peer and join its threads within `bound`.
    ///
    /// # Errors
    ///
    /// Names the threads still running after the bound.
    pub fn finish(self, bound: Duration) -> Result<(), String> {
        self.threads.finish(bound)
    }

    /// Finish the peer after a row, within `bound`, keeping the row's own
    /// verdict first.
    ///
    /// # Errors
    ///
    /// The row's failure, then the threads still running after the bound.
    pub fn finished(self, bound: Duration, verdict: Row) -> Row {
        self.threads.finished(bound, verdict)
    }
}

impl PeerControl {
    /// Change how the peer answers from now on.
    ///
    /// `Script::Refuse` also drops every open connection.
    pub fn script(&self, script: Script) {
        self.shared().update(|state| {
            state.script = script;
            if script == Script::Refuse {
                state.cut();
            }
        });
    }

    /// Deliver `payloads` to subscription `sid` on the newest connection, in
    /// one write, so the SDK reads them together.
    ///
    /// # Errors
    ///
    /// When no connection is open or the write fails.
    pub fn deliver(&self, sid: &str, subject: &str, payloads: &[&[u8]]) -> Result<(), String> {
        let mut frames = Vec::new();
        for payload in payloads {
            Reply::Message(payload).encode(subject, sid, &mut frames);
        }
        self.shared()
            .state()
            .newest()?
            .write(&frames)
            .map_err(|error| format!("deliver to {sid}: {error}"))
    }

    /// Write `reply` on `subject` to every live subscription on the newest
    /// connection whose subject matches it, in one write, as a server routes
    /// it: exact, `*`, and `>` subjects alike, one member per queue group
    /// name across every matching subject. Where a server picks any member,
    /// the peer picks the first subscribed.
    ///
    /// Returns the subscription IDs written to, in subscription order.
    ///
    /// # Errors
    ///
    /// When no connection is open, no live subscription matches, or the
    /// write fails.
    pub fn reply(&self, subject: &str, reply: &Reply<'_>) -> Result<Box<[Box<str>]>, String> {
        self.shared().state().newest()?.route(subject, reply)
    }

    /// Write a `-ERR` naming `message` on the newest connection and keep it
    /// open, as a server reports a connection-wide refusal such as a
    /// permissions violation. It answers no publication.
    ///
    /// # Errors
    ///
    /// When no connection is open or the write fails.
    pub fn server_error(&self, message: &str) -> Row {
        self.shared()
            .state()
            .newest()?
            .write(format!("-ERR '{message}'\r\n").as_bytes())
    }

    /// The first `count` complete publications, once the peer read them.
    ///
    /// # Errors
    ///
    /// When fewer arrive within `bound`.
    pub fn publications(
        &self,
        count: usize,
        bound: Duration,
    ) -> Result<Box<[Publication]>, String> {
        let log = self.wait_for(&format!("{count} publications"), bound, |log| {
            log.publications.len() >= count
        })?;
        Ok(log.publications.into_iter().take(count).collect())
    }

    /// Wait for the SDK to process all preceding deliveries on this connection.
    pub fn delivery_barrier(&self, bound: Duration) -> Row {
        let previous = self.log().pongs;
        self.shared().state().newest()?.write(b"PING\r\n")?;
        self.wait_for("delivery PONG", bound, |log| log.pongs > previous)
            .map(|_| ())
    }

    /// The ID of the newest subscription to `subject`, once the peer read it.
    ///
    /// # Errors
    ///
    /// When no such subscription arrives within `bound`.
    pub fn sid(&self, subject: &str, bound: Duration) -> Result<Box<str>, String> {
        let log = self.wait_for(&format!("a subscription to {subject}"), bound, |log| {
            log.subscribed
                .iter()
                .any(|(recorded, _, _)| &**recorded == subject)
        })?;
        log.subscribed
            .iter()
            .rev()
            .find(|(recorded, _, _)| &**recorded == subject)
            .map(|(_, _, sid)| sid.clone())
            .ok_or_else(|| format!("no subscription to {subject}"))
    }
}

/// Stall the transport under one oversized publish the peer sees buffered.
///
/// The SDK takes no command while that write is unfinished, so every
/// operation submitted after this returns holds until the peer reads again.
///
/// # Errors
///
/// When the publish finished at once, or the peer saw nothing buffered
/// within [`ROW_BOUND`].
pub fn hold_transport<'a>(
    connection: &'a Connection,
    control: &PeerControl,
    payload: &'a [u8],
) -> Result<Pin<Box<impl Future<Output = Result<(), RuntimeError>> + use<'a>>>, String> {
    control.script(Script::Stall);
    let mut held = Box::pin(connection.publish("big", payload));
    expect_stalled_publish(control, held.as_mut())?;
    Ok(held)
}

/// Fail the row unless the admitted publish `held` is pending and the peer
/// saw its bytes buffered unread.
///
/// # Errors
///
/// When the publish finished at once, or the peer saw nothing buffered
/// within [`ROW_BOUND`].
pub fn expect_stalled_publish(
    control: &PeerControl,
    held: Pin<&mut impl Future<Output = Result<(), RuntimeError>>>,
) -> Row {
    expect_pending("the stalled publish", held)?;
    control
        .wait_for("the stalled publish buffered", ROW_BOUND, |log| log.unread)
        .map(drop)
}

/// Poll the connection's committed readiness until the SDK reports it
/// disconnected, under the hang guard `bound`.
///
/// # Errors
///
/// When the guard passes first.
pub fn wait_unavailable(connection: &Connection, bound: Duration) -> Row {
    bounded("the SDK disconnect", bound, async {
        loop {
            match connection.ready().map_err(|error| refusal(&error)) {
                Err(Some((_, IntegrationFailure::Unavailable, _))) => return,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
}

/// Record one accepted connection and serve it on its own thread, unless the
/// script refuses it.
fn admit(stream: TcpStream, shared: &Arc<Shared>, spawned: &Spawned) {
    let refused = shared.update(|state| {
        state.log.accepted += 1;
        state.script == Script::Refuse
    });
    if refused {
        drop(stream.shutdown(Shutdown::Both));
        return;
    }
    let shared = Arc::clone(shared);
    spawn_tracked(spawned, move || serve(stream, &shared));
}

/// Serve one connection until it closes or the peer stops, then forget it.
fn serve(mut stream: TcpStream, shared: &Shared) {
    // An accepted socket can inherit the listener's nonblocking mode.
    let (Ok(()), Ok(writer)) = (stream.set_nonblocking(false), stream.try_clone()) else {
        return;
    };
    let link = shared.update(|state| {
        let id = state.next_link;
        state.next_link += 1;
        state.links.push(Link {
            id,
            writer,
            routes: Vec::new(),
        });
        id
    });
    exchange(&mut stream, shared, link);
    shared.update(|state| state.close(link));
}

/// Answer the handshake on connection `link`, then read and answer its
/// commands until it ends or the peer stops.
fn exchange(stream: &mut TcpStream, shared: &Shared, link: usize) {
    let info = format!(
        "INFO {{\"server_id\":\"scripted\",\"server_name\":\"scripted\",\"version\":\"2.10.0\",\"go\":\"go1.22\",\"host\":\"127.0.0.1\",\"port\":4222,\"headers\":true,\"max_payload\":{ADVERTISED_MAX_PAYLOAD},\"proto\":1}}\r\n"
    );
    if !answered_handshake(shared) {
        return;
    }
    // Only the client can end the connection here: the peer's own stop and
    // refusal commit their state first. A client that ended before the
    // handshake fails the write, or the read timeout on macOS (`EINVAL`).
    if stream.write_all(info.as_bytes()).is_err() || stream.set_read_timeout(Some(POLL)).is_err() {
        shared.update(|state| record_client_close(state, link));
        return;
    }
    let mut buffer = Vec::new();
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        let (script, stopping) = {
            let state = shared.state();
            (state.script, state.stopping)
        };
        match script {
            _ if stopping => return,
            // A stalled connection reads nothing until the client ends it.
            Script::Stall if stalled_open(stream, &mut chunk, shared, link) => continue,
            Script::Stall | Script::Refuse => return,
            Script::Serve | Script::Silent | Script::DenyAuthorization => {}
        }
        let read = match stream.read(&mut chunk) {
            Err(error) if read_timed_out(&error) => continue,
            Ok(0) | Err(_) => {
                shared.update(|state| record_client_close(state, link));
                return;
            }
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                read
            }
        };
        let framed = handle_commands(&mut buffer, stream, shared, link);
        let served = shared.update(|state| {
            state.log.received += read;
            framed.map_err(|reason| refuse_frame(state, link, reason))
        });
        match served {
            Ok(Served::Open) => {}
            Ok(Served::Denied) => return,
            Err(error) => {
                drop(stream.write_all(error.as_bytes()));
                drop(stream.shutdown(Shutdown::Both));
                return;
            }
        }
    }
}

/// Record whether bytes wait unread on a stalled connection, without reading
/// them.
///
/// Returns `false` once the client ended the connection.
fn stalled_open(stream: &TcpStream, chunk: &mut [u8], shared: &Shared, link: usize) -> bool {
    match stream.peek(chunk) {
        Err(error) if read_timed_out(&error) => true,
        Ok(0) | Err(_) => {
            shared.update(|state| record_client_close(state, link));
            false
        }
        Ok(_) => {
            shared.update(|state| state.log.unread = true);
            std::thread::park_timeout(POLL);
            true
        }
    }
}

/// Forget connection `link` and count its end if the client caused it: the
/// peer's own shutdowns, on stop or refusal, commit their state before they
/// close the socket, and a denied or refused connection is never read again.
fn record_client_close(state: &mut State, link: usize) {
    state.close(link);
    if !state.stopping && state.script != Script::Refuse {
        state.log.closed += 1;
    }
}

/// Hold a connection accepted under `Script::Silent` without answering,
/// until the script changes or the peer stops.
///
/// Returns whether the connection should go on to its handshake.
fn answered_handshake(shared: &Shared) -> bool {
    let mut state = shared.state();
    while state.script == Script::Silent && !state.stopping {
        state = shared.wait_timeout(state, POLL);
    }
    !state.stopping && state.script != Script::Refuse
}

/// Whether a connection goes on after a command.
enum Served {
    /// Keep reading.
    Open,
    /// The peer denied the connection and already forgot it.
    Denied,
}

/// Answer every complete command at the head of `buffer` on `stream`, then
/// keep only the incomplete rest. A denial ends the connection, so nothing
/// after it is answered.
///
/// # Errors
///
/// Why a frame was malformed; the buffer is then left as it was read.
fn handle_commands(
    buffer: &mut Vec<u8>,
    stream: &mut TcpStream,
    shared: &Shared,
    link: usize,
) -> Result<Served, Box<str>> {
    loop {
        let (command, consumed) = match wire::parse(buffer) {
            Frame::Incomplete => return Ok(Served::Open),
            Frame::Malformed(reason) => return Err(reason),
            Frame::Complete { command, consumed } => (command, consumed),
        };
        let served = answer(command, stream, shared, link);
        buffer.drain(..consumed);
        if let Served::Denied = served {
            return Ok(served);
        }
    }
}

/// Record one complete command, answering on `stream` where the protocol
/// asks.
fn answer(command: Command, stream: &mut TcpStream, shared: &Shared, link: usize) -> Served {
    match command {
        Command::Publish(publication) => {
            shared.update(|state| record_publication(state, publication));
        }
        Command::Connect => return record_connect(stream, shared, link),
        Command::Ping => {
            shared.update(|state| state.log.pings += 1);
            drop(stream.write_all(b"PONG\r\n"));
        }
        Command::Pong => shared.update(|state| state.log.pongs += 1),
        Command::Subscribe(subscription) => {
            shared.update(|state| record_subscribe(state, link, subscription));
        }
        Command::Unsubscribe { sid, max } => {
            shared.update(|state| record_unsubscribe(state, link, sid, max));
        }
        Command::Other => {}
    }
    Served::Open
}

/// Record one complete publication in both the Core log and in full.
fn record_publication(state: &mut State, publication: Publication) {
    state
        .log
        .published
        .push((publication.subject.clone(), publication.payload.len()));
    state.log.publications.push(publication);
}

/// Refuse a malformed frame on connection `link` as a server would: record
/// why and forget the connection before anything reaches the wire.
///
/// Returns the `-ERR` line the connection ends with.
fn refuse_frame(state: &mut State, link: usize, reason: Box<str>) -> String {
    let error = format!("-ERR '{reason}'\r\n");
    state.log.malformed.push(reason);
    state.close(link);
    error
}

/// Record one `CONNECT` on connection `link`, and deny it when the script
/// says so: forget the connection before the `-ERR` reaches the wire.
fn record_connect(stream: &mut TcpStream, shared: &Shared, link: usize) -> Served {
    let deny = shared.update(|state| {
        state.log.connects += 1;
        let deny = state.script == Script::DenyAuthorization;
        if deny {
            state.close(link);
        }
        deny
    });
    if !deny {
        return Served::Open;
    }
    drop(stream.write_all(b"-ERR 'Authorization Violation'\r\n"));
    drop(stream.shutdown(Shutdown::Both));
    Served::Denied
}

/// Record one `SUB`, and route replies on connection `link` to it.
fn record_subscribe(state: &mut State, link: usize, subscription: Subscription) {
    let (subject, queue, sid) = &subscription;
    if let Some(open) = state.link(link) {
        open.routes.push(Route {
            subject: subject.clone(),
            queue: queue.clone(),
            sid: sid.clone(),
            delivered: 0,
            max: None,
        });
    }
    state.log.subscribed.push(subscription);
}

/// Record one `UNSUB`: end the route at once, or after `max` deliveries in
/// all.
fn record_unsubscribe(state: &mut State, link: usize, sid: Box<str>, max: Option<usize>) {
    if let Some(open) = state.link(link) {
        for route in open.routes.iter_mut().filter(|route| route.sid == sid) {
            route.max = Some(max.unwrap_or(0));
        }
        open.routes.retain(Route::live);
    }
    state.log.unsubscribed.push(sid);
}
