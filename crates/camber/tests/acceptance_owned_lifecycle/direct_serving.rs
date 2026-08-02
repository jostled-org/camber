use crate::common;

use std::future::IntoFuture;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, lifecycle};
use camber::http::{HostRouter, Request, Response, Router, SseWriter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread")]
async fn serve_async_handles_request() {
    let mut router = Router::new();
    router.get("/", |_req: &Request| async { Response::text(200, "hello") });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(camber::http::serve_async(listener, router));

    let response =
        tokio::time::timeout(EVENT_TIMEOUT, reqwest::get(format!("http://{addr}/"))).await;
    server.abort();
    let server_join = tokio::time::timeout(EVENT_TIMEOUT, server).await;
    assert!(server_join.unwrap().unwrap_err().is_cancelled());
    let resp = response.unwrap().unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_async_sse_stream() {
    let mut router = Router::new();
    router.get_sse("/events", |_req: &Request, writer: &mut SseWriter| {
        for i in 0..3 {
            writer.event("message", &format!("data-{i}"))?;
        }
        Ok(())
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(camber::http::serve_async(listener, router));

    // Use a blocking task for raw TCP SSE reading
    let events = tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(stream);

        // Skip HTTP response line and headers
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
        }

        read_sse_events(&mut reader, 3)
    });
    let events = tokio::time::timeout(EVENT_TIMEOUT, events).await;
    server.abort();
    let server_join = tokio::time::timeout(EVENT_TIMEOUT, server).await;
    assert!(server_join.unwrap().unwrap_err().is_cancelled());
    let events = events.unwrap().unwrap();

    assert_eq!(events.len(), 3);
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event, &format!("event: message\ndata: data-{i}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_async_shares_tokio_runtime() {
    let (tx, rx) = tokio::sync::oneshot::channel::<tokio::task::JoinHandle<bool>>();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let mut router = Router::new();
    let tx_clone = Arc::clone(&tx);
    router.get("/check", move |_req: &Request| {
        // Spawn a tokio task from within the handler — proves we share the runtime
        let sender = tx_clone.lock().unwrap_or_else(|e| e.into_inner()).take();
        async move {
            match sender {
                Some(tx) => {
                    let task = tokio::runtime::Handle::current().spawn(async { true });
                    let _ = tx.send(task);
                    Response::text(200, "spawned")
                }
                None => Response::text(200, "already sent"),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(camber::http::serve_async(listener, router));

    let response =
        tokio::time::timeout(EVENT_TIMEOUT, reqwest::get(format!("http://{addr}/check"))).await;

    // The handler spawned a tokio task that sends through the oneshot.
    // If serve_async created a separate runtime, this would never resolve.
    let task = tokio::time::timeout(Duration::from_secs(2), rx).await;
    let result = match task {
        Ok(Ok(task)) => Some(tokio::time::timeout(EVENT_TIMEOUT, task).await),
        Ok(Err(_)) | Err(_) => None,
    };
    server.abort();
    let server_join = tokio::time::timeout(EVENT_TIMEOUT, server).await;
    assert!(server_join.unwrap().unwrap_err().is_cancelled());
    let resp = response.unwrap().unwrap();
    assert_eq!(resp.status(), 200);
    let result = result
        .expect("handler did not return its spawned task")
        .expect("timeout joining spawned task")
        .expect("spawned task panicked");
    assert!(result);
}

#[test]
fn serve_async_variants_capture_first_poll_context_and_own_transports() {
    direct_variants_join_transports_on_camber_shutdown();
    camber_creation_plain_first_poll_uses_standalone_context();
    plain_creation_camber_first_poll_captures_runtime_context();
    dropping_pure_tokio_direct_future_aborts_owned_transports();
}

struct DirectVariant {
    name: &'static str,
    gate: GateControl,
    client: tokio::task::JoinHandle<Vec<u8>>,
    server: camber::AsyncJoinHandle<Result<(), RuntimeError>>,
}

async fn assert_direct_variants_shutdown(variants: [DirectVariant; 4]) {
    let [plain, tls, hosts, hosts_tls] = variants;
    let names = [plain.name, tls.name, hosts.name, hosts_tls.name];
    let mut gates = [
        plain.gate.entered().await,
        tls.gate.entered().await,
        hosts.gate.entered().await,
        hosts_tls.gate.entered().await,
    ];
    let clients = [plain.client, tls.client, hosts.client, hosts_tls.client];
    let mut servers = [
        Box::pin(plain.server.into_future()),
        Box::pin(tls.server.into_future()),
        Box::pin(hosts.server.into_future()),
        Box::pin(hosts_tls.server.into_future()),
    ];

    camber::runtime::request_shutdown();
    for (server, name) in servers.iter_mut().zip(names) {
        assert_server_remains_pending(server, name).await;
    }
    gates.iter().for_each(EnteredGate::release);
    for (client, name) in clients.into_iter().zip(names) {
        assert_http_ok(join_client(client).await, name);
    }
    for gate in &mut gates {
        gate.wait_for_drop().await;
    }
    for (server, name) in servers.into_iter().zip(names) {
        assert_camber_server_ok(join_camber_server(server).await, name);
    }
}

fn direct_variants_join_transports_on_camber_shutdown() {
    camber::runtime::builder()
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            camber::runtime::block_on(async {
                let (tls_config, connector) = common::self_signed_server_and_connector();

                let (plain_router, plain_gate) = gated_router();
                let plain_listener = bind_listener().await;
                let plain_addr = plain_listener.local_addr().unwrap();
                let plain_server =
                    camber::spawn_async(camber::http::serve_async(plain_listener, plain_router));

                let (tls_router, tls_gate) = gated_router();
                let tls_listener = bind_listener().await;
                let tls_addr = tls_listener.local_addr().unwrap();
                let tls_server = camber::spawn_async(camber::http::serve_async_tls(
                    tls_listener,
                    tls_router,
                    Arc::clone(&tls_config),
                ));

                let (hosts_router, hosts_gate) = gated_router();
                let hosts_listener = bind_listener().await;
                let hosts_addr = hosts_listener.local_addr().unwrap();
                let hosts_server = camber::spawn_async(camber::http::serve_async_hosts(
                    hosts_listener,
                    host_router(hosts_router),
                ));

                let (hosts_tls_router, hosts_tls_gate) = gated_router();
                let hosts_tls_listener = bind_listener().await;
                let hosts_tls_addr = hosts_tls_listener.local_addr().unwrap();
                let hosts_tls_server = camber::spawn_async(camber::http::serve_async_hosts_tls(
                    hosts_tls_listener,
                    host_router(hosts_tls_router),
                    tls_config,
                ));

                let plain_client = tokio::spawn(plain_request(plain_addr, "localhost"));
                let tls_client =
                    tokio::spawn(tls_request(connector.clone(), tls_addr, "localhost"));
                let hosts_client = tokio::spawn(plain_request(hosts_addr, "direct.test"));
                let hosts_tls_client =
                    tokio::spawn(tls_request(connector, hosts_tls_addr, "direct.test"));

                assert_direct_variants_shutdown([
                    DirectVariant {
                        name: "plain",
                        gate: plain_gate,
                        client: plain_client,
                        server: plain_server,
                    },
                    DirectVariant {
                        name: "TLS",
                        gate: tls_gate,
                        client: tls_client,
                        server: tls_server,
                    },
                    DirectVariant {
                        name: "host",
                        gate: hosts_gate,
                        client: hosts_client,
                        server: hosts_server,
                    },
                    DirectVariant {
                        name: "host-plus-TLS",
                        gate: hosts_tls_gate,
                        client: hosts_tls_client,
                        server: hosts_tls_server,
                    },
                ])
                .await;
            })
        })
        .unwrap();
}

fn camber_creation_plain_first_poll_uses_standalone_context() {
    camber::runtime::builder()
        .connection_limit(1)
        .shutdown_timeout(Duration::from_millis(150))
        .run(|| {
            camber::runtime::block_on(async {
                let (router, mut gate) = multi_gated_router();
                let listener = bind_listener().await;
                let addr = listener.local_addr().unwrap();

                // Calling an async fn creates its future here, under Camber.
                let server = camber::http::serve_async(listener, router);
                // A raw Tokio task has no Camber provenance on its first poll.
                let mut server = tokio::spawn(server);

                let first_client = tokio::spawn(plain_request(addr, "localhost"));
                receive_with_guard(&mut gate.entered, "first standalone transport").await;

                camber::runtime::request_shutdown();

                let second_client = tokio::spawn(plain_request(addr, "localhost"));
                let third_client = tokio::spawn(plain_request(addr, "localhost"));
                receive_with_guard(&mut gate.entered, "second standalone transport").await;
                receive_with_guard(&mut gate.entered, "third standalone transport").await;

                let configured_deadline =
                    tokio::time::timeout(Duration::from_millis(300), &mut server).await;
                assert!(
                    configured_deadline.is_err(),
                    "Camber creation context supplied shutdown, timeout, or connection-limit state"
                );

                server.abort();
                let join_error = tokio::time::timeout(EVENT_TIMEOUT, server)
                    .await
                    .expect("aborted direct future did not join")
                    .expect_err("aborted direct future returned a result");
                assert!(join_error.is_cancelled());
                wait_for_drops(&mut gate.dropped, 3, "standalone transport").await;

                join_client(first_client).await;
                join_client(second_client).await;
                join_client(third_client).await;
            })
        })
        .unwrap();
}

fn plain_creation_camber_first_poll_captures_runtime_context() {
    camber::runtime::builder()
        .connection_limit(1)
        .shutdown_timeout(Duration::from_millis(150))
        .run(|| {
            camber::runtime::block_on(async {
                let (router, mut gate) = multi_gated_router();
                let listener = bind_listener().await;
                let addr = listener.local_addr().unwrap();
                let controller = lifecycle(addr).unwrap();

                // The raw task creates, but does not poll, the direct future.
                let server = tokio::time::timeout(
                    EVENT_TIMEOUT,
                    tokio::spawn(async move { camber::http::serve_async(listener, router) }),
                )
                .await
                .unwrap()
                .unwrap();
                let server = camber::spawn_async(server);

                let first_client = tokio::spawn(plain_request(addr, "localhost"));
                receive_with_guard(&mut gate.entered, "first Camber transport").await;

                controller
                    .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .unwrap();
                let second_client = tokio::spawn(plain_request(addr, "localhost"));
                controller
                    .wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .await
                    .unwrap();
                assert!(
                    matches!(
                        gate.entered.try_recv(),
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                    ),
                    "second transport dispatched despite captured connection_limit(1)"
                );

                camber::runtime::request_shutdown();
                controller
                    .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .unwrap();

                let result = tokio::time::timeout(Duration::from_secs(2), server)
                    .await
                    .expect("captured Camber timeout did not expire")
                    .expect("Camber owner task failed before returning its serve result");
                assert!(
                    matches!(result, Err(RuntimeError::Timeout)),
                    "expected captured 150ms timeout, got {result:?}"
                );
                assert_eq!(
                    gate.dropped.try_recv(),
                    Ok(()),
                    "server returned before aborting its active transport"
                );
                assert!(
                    matches!(
                        gate.entered.try_recv(),
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
                    ),
                    "owner completion retained the router's handler sender"
                );

                join_client(first_client).await;
                join_client(second_client).await;
            })
        })
        .unwrap();
}

fn dropping_pure_tokio_direct_future_aborts_owned_transports() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (router, mut gate) = multi_gated_router();
        let listener = bind_listener().await;
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(camber::http::serve_async(listener, router));
        let client = tokio::spawn(plain_request(addr, "localhost"));

        receive_with_guard(&mut gate.entered, "pure-Tokio transport").await;
        server.abort();
        let join_error = tokio::time::timeout(EVENT_TIMEOUT, server)
            .await
            .expect("aborted pure-Tokio server did not join")
            .expect_err("dropping the direct future claimed a serve join result");
        assert!(join_error.is_cancelled());
        receive_with_guard(&mut gate.dropped, "aborted pure-Tokio transport").await;
        join_client(client).await;
    });
}

struct GateControl {
    entered: tokio::sync::oneshot::Receiver<()>,
    release: Arc<tokio::sync::Semaphore>,
    dropped: tokio::sync::oneshot::Receiver<()>,
}

impl GateControl {
    async fn entered(self) -> EnteredGate {
        let GateControl {
            mut entered,
            release,
            dropped,
        } = self;
        tokio::time::timeout(Duration::from_secs(2), &mut entered)
            .await
            .expect("timed out waiting for gated transport")
            .expect("gated transport ended before entering its handler");
        EnteredGate { release, dropped }
    }
}

struct EnteredGate {
    release: Arc<tokio::sync::Semaphore>,
    dropped: tokio::sync::oneshot::Receiver<()>,
}

impl EnteredGate {
    fn release(&self) {
        self.release.add_permits(1);
    }

    async fn wait_for_drop(&mut self) {
        tokio::time::timeout(Duration::from_secs(2), &mut self.dropped)
            .await
            .expect("transport ownership probe remained live")
            .expect("transport ownership probe sender disappeared");
    }
}

struct MultiGateControl {
    entered: tokio::sync::mpsc::UnboundedReceiver<()>,
    dropped: tokio::sync::mpsc::UnboundedReceiver<()>,
}

struct OneDropSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for OneDropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

struct ManyDropSignal(Option<tokio::sync::mpsc::UnboundedSender<()>>);

impl Drop for ManyDropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

fn gated_router() -> (Router, GateControl) {
    let (entered_sender, entered) = tokio::sync::oneshot::channel();
    let (dropped_sender, dropped) = tokio::sync::oneshot::channel();
    let signals = Arc::new(Mutex::new(Some((entered_sender, dropped_sender))));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let handler_release = Arc::clone(&release);
    let mut router = Router::new();

    router.get("/", move |_request: &Request| {
        let (entered, dropped) = signals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("gated route received more than one request");
        let release = Arc::clone(&handler_release);
        async move {
            let _drop_signal = OneDropSignal(Some(dropped));
            let _ = entered.send(());
            let permit = release.acquire_owned().await.unwrap();
            permit.forget();
            Response::text(200, "released")
        }
    });

    (
        router,
        GateControl {
            entered,
            release,
            dropped,
        },
    )
}

fn multi_gated_router() -> (Router, MultiGateControl) {
    let (entered_sender, entered) = tokio::sync::mpsc::unbounded_channel();
    let (dropped_sender, dropped) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut router = Router::new();

    router.get("/", move |_request: &Request| {
        let entered = entered_sender.clone();
        let dropped = dropped_sender.clone();
        let release = Arc::clone(&release);
        async move {
            let _drop_signal = ManyDropSignal(Some(dropped));
            let _ = entered.send(());
            let permit = release.acquire_owned().await.unwrap();
            permit.forget();
            Response::text(200, "released")
        }
    });

    (router, MultiGateControl { entered, dropped })
}

fn host_router(router: Router) -> HostRouter {
    let mut hosts = HostRouter::new();
    hosts.add("direct.test", router);
    hosts
}

async fn bind_listener() -> tokio::net::TcpListener {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()
}

async fn plain_request(addr: SocketAddr, host: &'static str) -> Vec<u8> {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    request_and_read_to_eof(stream, host).await
}

async fn tls_request(
    connector: tokio_rustls::TlsConnector,
    addr: SocketAddr,
    host: &'static str,
) -> Vec<u8> {
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let stream = connector.connect(server_name, stream).await.unwrap();
    request_and_read_to_eof(stream, host).await
}

async fn request_and_read_to_eof<S>(mut stream: S, host: &str) -> Vec<u8>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    response
}

async fn assert_server_remains_pending<F>(server: &mut F, variant: &str)
where
    F: std::future::Future + Unpin,
{
    assert!(
        tokio::time::timeout(Duration::from_millis(100), server)
            .await
            .is_err(),
        "{variant} direct future returned while its accepted transport was active"
    );
}

async fn join_client(client: tokio::task::JoinHandle<Vec<u8>>) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(2), client)
        .await
        .expect("client did not observe transport closure")
        .expect("client task panicked")
}

async fn join_camber_server<F>(server: F) -> Result<Result<(), RuntimeError>, RuntimeError>
where
    F: std::future::Future<Output = Result<Result<(), RuntimeError>, RuntimeError>>,
{
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("direct server did not finish after transport closure")
}

fn assert_camber_server_ok(result: Result<Result<(), RuntimeError>, RuntimeError>, variant: &str) {
    match result {
        Ok(Ok(())) => {}
        other => panic!("{variant} direct future returned {other:?}"),
    }
}

fn assert_http_ok(response: Vec<u8>, variant: &str) {
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "{variant} transport did not finish its response: {response}"
    );
}

async fn receive_with_guard(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<()>, event: &str) {
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {event}"))
        .unwrap_or_else(|| panic!("channel closed while waiting for {event}"));
}

async fn wait_for_drops(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
    count: usize,
    transport: &str,
) {
    for _ in 0..count {
        receive_with_guard(receiver, transport).await;
    }
}

/// Read SSE events from a buffered reader. Each event ends with a blank line.
/// Skips HTTP chunked transfer encoding framing (hex size lines).
fn read_sse_events(reader: &mut BufReader<std::net::TcpStream>, count: usize) -> Vec<String> {
    let mut events = Vec::new();
    let mut current = String::new();
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(&['\r', '\n'][..]);

        // Skip chunked transfer encoding size lines (hex digits only)
        if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }

        match (
            trimmed.is_empty(),
            current.is_empty(),
            events.len().saturating_add(1) >= count,
        ) {
            (true, false, true) => {
                events.push(std::mem::take(&mut current));
                break;
            }
            (true, false, false) => events.push(std::mem::take(&mut current)),
            (true, true, _) => {}
            (false, _, _) => append_sse_line(&mut current, trimmed),
        }
    }
    events
}

fn append_sse_line(current: &mut String, line: &str) {
    match current.is_empty() {
        true => {}
        false => current.push('\n'),
    }
    current.push_str(line);
}
