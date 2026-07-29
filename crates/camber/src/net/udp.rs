use crate::RuntimeError;
use crate::runtime_state::LifecycleSignals;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

/// Receive buffer size: the largest value the UDP length field can hold.
///
/// Deliberately oversized. A UDP payload tops out at 65507 bytes once the
/// 8-byte UDP and 20-byte IPv4 headers are subtracted from that field's range,
/// so no datagram the socket can deliver is ever truncated here.
const MAX_DATAGRAM: usize = 65535;

/// Async UDP socket wrapping `tokio::net::UdpSocket`.
#[derive(Debug)]
pub struct UdpSocket {
    inner: tokio::net::UdpSocket,
}

impl UdpSocket {
    /// Bind a UDP socket to the given address.
    pub async fn bind(addr: &str) -> Result<Self, RuntimeError> {
        let inner = tokio::net::UdpSocket::bind(addr).await?;
        Ok(Self { inner })
    }

    /// Connect to a remote address for use with `send`/`recv`.
    pub async fn connect(&self, addr: &str) -> Result<(), RuntimeError> {
        self.inner.connect(addr).await?;
        Ok(())
    }

    /// Send a datagram to the specified address.
    ///
    /// `target` is anything Tokio resolves, so a reply can name the
    /// [`SocketAddr`] the recv loop just handed the handler. A `&str` still
    /// works and still resolves the same way; the address form simply skips the
    /// formatting and re-parsing of an address that was already parsed, on the
    /// one call every datagram makes.
    pub async fn send_to<A>(&self, datagram: &[u8], target: A) -> Result<usize, RuntimeError>
    where
        A: tokio::net::ToSocketAddrs,
    {
        let bytes_sent = self.inner.send_to(datagram, target).await?;
        Ok(bytes_sent)
    }

    /// Receive a datagram, returning the number of bytes read and the sender address.
    pub async fn recv_from(
        &self,
        recv_buf: &mut [u8],
    ) -> Result<(usize, SocketAddr), RuntimeError> {
        let (bytes_read, addr) = self.inner.recv_from(recv_buf).await?;
        Ok((bytes_read, addr))
    }

    /// Send a datagram on a connected socket.
    pub async fn send(&self, datagram: &[u8]) -> Result<usize, RuntimeError> {
        let bytes_sent = self.inner.send(datagram).await?;
        Ok(bytes_sent)
    }

    /// Receive a datagram on a connected socket.
    pub async fn recv(&self, recv_buf: &mut [u8]) -> Result<usize, RuntimeError> {
        let bytes_read = self.inner.recv(recv_buf).await?;
        Ok(bytes_read)
    }

    /// Returns the local address this socket is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, RuntimeError> {
        let addr = self.inner.local_addr()?;
        Ok(addr)
    }
}

/// Bind a UDP socket to `addr` and run a recv loop dispatching datagrams to `handler`.
///
/// Runs until the runtime asks it to stop — a shutdown request, or the root
/// scope closing — then returns `Ok(())`.
/// The handler runs inline — no per-datagram spawn. If concurrency is needed,
/// spawn inside the handler.
///
/// With no runtime context the lifecycle signals are inert latches that nothing
/// can fire, so that stop condition is unreachable and the loop runs until the
/// caller drops the future. Absence is not refused here because the caller owns
/// this future and can end it that way.
pub async fn serve_udp<F, Fut>(addr: &str, handler: F) -> Result<(), RuntimeError>
where
    F: Fn(Vec<u8>, SocketAddr, Arc<UdpSocket>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send,
{
    let socket = UdpSocket::bind(addr).await?;
    serve_udp_on(socket, handler).await
}

/// Run a recv loop on an existing UDP socket, dispatching datagrams to `handler`.
///
/// Runs until the runtime asks it to stop — a shutdown request, or the root
/// scope closing — then returns `Ok(())`.
/// The handler runs inline — no per-datagram spawn.
///
/// With no runtime context the lifecycle signals are inert latches that nothing
/// can fire, so that stop condition is unreachable and the loop runs until the
/// caller drops the future. Absence is not refused here because the caller owns
/// this future and can end it that way.
pub async fn serve_udp_on<F, Fut>(socket: UdpSocket, handler: F) -> Result<(), RuntimeError>
where
    F: Fn(Vec<u8>, SocketAddr, Arc<UdpSocket>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send,
{
    let signals = LifecycleSignals::current();
    let socket = Arc::new(socket);

    recv_loop(&socket, &signals, &handler).await
}

/// Receive datagrams until either lifecycle signal fires.
///
/// The pair, not the shutdown latch alone. The user closure's return closes
/// root-scope admission and fires `ScopeClosing` with the shutdown latch left
/// unset, so a binding backgrounded with `camber::n` would otherwise sit on a
/// latch nobody fires and turn a clean exit into a scope drain timeout.
async fn recv_loop<F, Fut>(
    socket: &Arc<UdpSocket>,
    signals: &LifecycleSignals,
    handler: &F,
) -> Result<(), RuntimeError>
where
    F: Fn(Vec<u8>, SocketAddr, Arc<UdpSocket>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send,
{
    // Heap-backed: a 64 KiB array here would live inside this future and be
    // copied through every enclosing future — including the caller's spawn.
    let mut buf = vec![0u8; MAX_DATAGRAM].into_boxed_slice();
    // Registered once for the binding, not once per datagram. Both latches are
    // sticky, so a wait that has not resolved is still the same wait next time
    // round; constructing it inside the loop would register and deregister a
    // `Notify` waiter — an internal mutex and an intrusive-list edit — on every
    // single datagram.
    let stop = signals.wait();
    tokio::pin!(stop);
    loop {
        // `biased` gives the stop signals priority over a ready datagram, the same
        // ordering the accept loop uses, and states the stop condition once.
        // An unbiased `select!` needs the check repeated before the receive
        // and again after it — and losing that second check consumes a
        // datagram only to discard it. Biased, the receive future is dropped
        // unpolled and the datagram stays queued on the socket.
        let received = tokio::select! {
            biased;
            () = &mut stop => return Ok(()),
            result = socket.recv_from(&mut buf) => result,
        };
        match classify_receive(received)? {
            None => {}
            Some((len, addr)) => dispatch(&buf[..len], addr, socket, handler).await,
        }
    }
}

/// Classify one receive outcome: a datagram to dispatch, or a transient
/// failure the binding survives.
///
/// A transient error ends this datagram, not the binding. Propagating it would
/// take the whole server down on a routine ICMP bounce.
fn classify_receive(
    result: Result<(usize, SocketAddr), RuntimeError>,
) -> Result<Option<(usize, SocketAddr)>, RuntimeError> {
    match result {
        Ok(received) => Ok(Some(received)),
        Err(error) if crate::error::is_transient_datagram_error(&error) => {
            tracing::debug!(%error, "udp recv: transient error, continuing");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Copy the datagram out of the shared receive buffer and run the handler.
///
/// A handler failure is reported against that datagram alone: the binding is
/// still good, so one bad message must not end the server. It reports through
/// the same triage every other transport's handler does, so a peer that hung up
/// is the ordinary end of an exchange here too.
async fn dispatch<F, Fut>(datagram: &[u8], addr: SocketAddr, socket: &Arc<UdpSocket>, handler: &F)
where
    F: Fn(Vec<u8>, SocketAddr, Arc<UdpSocket>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send,
{
    let result = handler(datagram.to_vec(), addr, Arc::clone(socket)).await;
    super::accept::report_handler_error("udp", result);
}
