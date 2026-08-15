use crate::common;

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, lifecycle};
use camber::http::{HostRouter, Request, Response, Router, ServerHandle, ServerHandleFuture};
use futures_util::FutureExt;
use std::future::IntoFuture;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

struct RetainedRequest {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
    ownership: Arc<()>,
    response: &'static str,
}

struct RetainedRoute {
    router: Router,
    entered: tokio::sync::oneshot::Receiver<()>,
    release: tokio::sync::oneshot::Sender<()>,
    ownership: Weak<()>,
}

fn retained_router(response: &'static str) -> RetainedRoute {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let ownership = Arc::new(());
    let ownership_probe = Arc::downgrade(&ownership);
    let retained = Mutex::new(Some(RetainedRequest {
        entered: entered_tx,
        release: release_rx,
        ownership,
        response,
    }));

    let mut router = Router::new();
    router.get("/retained", move |_req: &Request| {
        let retained = retained
            .lock()
            .unwrap()
            .take()
            .expect("retained route called more than once");
        async move {
            retained.entered.send(()).unwrap();
            let _ = retained.release.await;
            drop(retained.ownership);
            Response::text(200, retained.response)
        }
    });

    RetainedRoute {
        router,
        entered: entered_rx,
        release: release_tx,
        ownership: ownership_probe,
    }
}

fn host_dispatch(router: Router) -> HostRouter {
    let mut host_router = HostRouter::new();
    host_router.set_default(router);
    host_router
}

async fn http_get(
    addr: std::net::SocketAddr,
    host: Option<&str>,
    path: &str,
) -> Result<(u16, String), reqwest::Error> {
    let request = reqwest::Client::new().get(format!("http://{addr}{path}"));
    let request = match host {
        Some(host) => request.header("host", host),
        None => request,
    };
    let response = request.send().await?;
    let status = response.status().as_u16();
    let body = response.text().await?;
    Ok((status, body))
}

async fn wait_for_checkpoint(controller: &LifecycleController, checkpoint: LifecycleCheckpoint) {
    tokio::time::timeout(EVENT_TIMEOUT, controller.wait_until_paused(checkpoint))
        .await
        .unwrap()
        .unwrap();
}

fn assert_flat_ok(result: Result<(), RuntimeError>, variant: &str) {
    assert!(result.is_ok(), "{variant} returned {result:?}");
}

/// Make an HTTPS GET request using hyper over TLS, returning (status, body).
async fn https_get(
    connector: &tokio_rustls::TlsConnector,
    addr: std::net::SocketAddr,
    path: &str,
) -> Result<(u16, String), Box<str>> {
    let req = hyper::Request::get(format!("http://localhost{path}"))
        .header("host", "localhost")
        .header("connection", "close")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .map_err(|error| format!("HTTP request build failed: {error}").into_boxed_str())?;
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|error| format!("TLS TCP connect failed: {error}").into_boxed_str())?;
    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .map_err(|error| format!("invalid TLS server name: {error}").into_boxed_str())?;
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|error| format!("TLS handshake failed: {error}").into_boxed_str())?;

    let io = hyper_util::rt::TokioIo::new(tls_stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|error| format!("HTTP handshake failed: {error}").into_boxed_str())?;
    let connection = tokio::spawn(conn);
    let exchange = async {
        let resp = sender
            .send_request(req)
            .await
            .map_err(|error| format!("HTTPS request failed: {error}").into_boxed_str())?;
        let status = resp.status().as_u16();
        use http_body_util::BodyExt;
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|error| format!("HTTPS body failed: {error}").into_boxed_str())?
            .to_bytes();
        let body = String::from_utf8(body.to_vec())
            .map_err(|error| format!("HTTPS body was not UTF-8: {error}").into_boxed_str())?;
        Ok::<_, Box<str>>((status, body))
    }
    .await;
    drop(sender);
    let driver = tokio::time::timeout(EVENT_TIMEOUT, connection)
        .await
        .map_err(|error| format!("HTTP connection join timed out: {error}").into_boxed_str())?
        .map_err(|error| format!("HTTP connection task failed: {error}").into_boxed_str())?
        .map_err(|error| format!("HTTP connection failed: {error}").into_boxed_str());
    match (exchange, driver) {
        (Ok(response), Ok(())) => Ok(response),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_async_tls_accepts_https_connection() {
    let (tls_config, connector) = common::self_signed_server_and_connector();

    let mut router = Router::new();
    router.get("/tls-hello", |_req: &Request| async {
        Response::text(200, "tls works")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(
        camber::http::serve_async_tls(listener, router, tls_config)
            .expect("owned server requires a Tokio runtime"),
    );

    let response = https_get(&connector, addr, "/tls-hello").await;

    server.abort();
    let join = tokio::time::timeout(EVENT_TIMEOUT, server).await.unwrap();
    assert!(join.unwrap_err().is_cancelled());

    let (status, body) = response.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, "tls works");
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_async_hosts_dispatches_by_host() {
    let mut router_a = Router::new();
    router_a.get("/who", |_req: &Request| async {
        Response::text(200, "host-a")
    });

    let mut router_b = Router::new();
    router_b.get("/who", |_req: &Request| async {
        Response::text(200, "host-b")
    });

    let mut host_router = HostRouter::new();
    host_router.add("a.test", router_a);
    host_router.add("b.test", router_b);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(
        camber::http::serve_async_hosts(listener, host_router)
            .expect("owned server requires a Tokio runtime"),
    );

    // Request with Host: a.test
    let response_a = http_get(addr, Some("a.test"), "/who").await;

    // Request with Host: b.test
    let response_b = http_get(addr, Some("b.test"), "/who").await;

    server.abort();
    let join = tokio::time::timeout(EVENT_TIMEOUT, server).await.unwrap();
    assert!(join.unwrap_err().is_cancelled());

    assert_eq!(response_a.unwrap(), (200, "host-a".to_owned()));
    assert_eq!(response_b.unwrap(), (200, "host-b".to_owned()));
}

#[camber::test]
async fn serve_background_tls_runs_in_background() {
    let (tls_config, connector) = common::self_signed_server_and_connector();

    let mut router = Router::new();
    router.get("/bg-tls", |_req: &Request| async {
        Response::text(200, "background tls")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background_tls(listener, router, tls_config)
        .expect("owned server requires a Tokio runtime");

    let response = https_get(&connector, addr, "/bg-tls").await;

    handle.cancel();
    let server_result = tokio::time::timeout(EVENT_TIMEOUT, handle).await.unwrap();
    assert!(matches!(server_result, Err(RuntimeError::Cancelled)));

    let (status, body) = response.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, "background tls");

    // After cancellation, new connections should fail
    let tcp_result = tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect(addr),
    )
    .await;
    match tcp_result {
        Ok(Ok(tcp)) => {
            let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let tls_result = connector.connect(server_name, tcp).await;
            assert!(
                tls_result.is_err(),
                "TLS handshake should fail after cancel"
            );
        }
        Ok(Err(_)) | Err(_) => {} // Connection refusal or timeout is expected.
    }
}

/// 1.T3: Background server handle exposes flat Result<(), RuntimeError>,
/// not nested Result<Result<(), RuntimeError>, JoinError>.
#[camber::test]
async fn serve_background_handle_exposes_flat_error() {
    let mut router = Router::new();
    router.get("/flat", |_req: &Request| async {
        Response::text(200, "flat")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handle = camber::http::serve_background(listener, router)
        .expect("owned server requires a Tokio runtime");

    handle.cancel();

    // The key assertion: `.await` returns Result<(), RuntimeError> directly,
    // not Result<Result<(), RuntimeError>, _>. If the type were nested,
    // this line would not compile.
    let result: Result<(), RuntimeError> =
        tokio::time::timeout(EVENT_TIMEOUT, handle).await.unwrap();
    assert!(
        result.is_err(),
        "expected Err(Cancelled) after cancel, got Ok"
    );
}

type VariantClient = tokio::task::JoinHandle<Result<(u16, String), Box<str>>>;
type VariantJoin = Pin<Box<ServerHandleFuture>>;

/// Identity and observation points for one background variant. Every consuming
/// stage borrows these; the one-shot resources travel in separate owned arrays.
struct VariantProof {
    name: &'static str,
    expected_body: &'static str,
    addr: std::net::SocketAddr,
    lifecycle: LifecycleController,
    ownership: Weak<()>,
}

type VariantProofs = [VariantProof; 4];

struct StartedVariant {
    proof: VariantProof,
    handle: ServerHandle,
    entered: tokio::sync::oneshot::Receiver<()>,
    release: tokio::sync::oneshot::Sender<()>,
}

async fn variant_listener() -> (
    tokio::net::TcpListener,
    std::net::SocketAddr,
    LifecycleController,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let lifecycle = lifecycle(addr).unwrap();
    lifecycle
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    (listener, addr, lifecycle)
}

async fn start_variant<S>(
    name: &'static str,
    expected_body: &'static str,
    route: RetainedRoute,
    serve: S,
) -> StartedVariant
where
    S: FnOnce(tokio::net::TcpListener, Router) -> ServerHandle,
{
    let (listener, addr, lifecycle) = variant_listener().await;
    let handle = serve(listener, route.router);
    StartedVariant {
        proof: VariantProof {
            name,
            expected_body,
            addr,
            lifecycle,
            ownership: route.ownership,
        },
        handle,
        entered: route.entered,
        release: route.release,
    }
}

async fn boxed_http_get(
    addr: std::net::SocketAddr,
    host: Option<&str>,
) -> Result<(u16, String), Box<str>> {
    http_get(addr, host, "/retained")
        .await
        .map_err(|error| format!("plain HTTP GET failed: {error:?}").into_boxed_str())
}

fn spawn_variant_clients(
    addrs: [std::net::SocketAddr; 4],
    connector: tokio_rustls::TlsConnector,
) -> [VariantClient; 4] {
    let [plain_addr, tls_addr, host_addr, host_tls_addr] = addrs;
    let tls_connector = connector.clone();
    [
        tokio::spawn(boxed_http_get(plain_addr, None)),
        tokio::spawn(async move { https_get(&tls_connector, tls_addr, "/retained").await }),
        tokio::spawn(boxed_http_get(host_addr, Some("localhost"))),
        tokio::spawn(async move { https_get(&connector, host_tls_addr, "/retained").await }),
    ]
}

struct BackgroundVariants {
    proofs: VariantProofs,
    handles: [ServerHandle; 4],
    entered: [tokio::sync::oneshot::Receiver<()>; 4],
    releases: [tokio::sync::oneshot::Sender<()>; 4],
    clients: [VariantClient; 4],
}

impl BackgroundVariants {
    async fn start() -> Self {
        let (tls_config, connector) = common::self_signed_server_and_connector();
        let plain = start_variant(
            "plain",
            "plain",
            retained_router("plain"),
            |listener, router| {
                camber::http::serve_background(listener, router)
                    .expect("owned server requires a Tokio runtime")
            },
        )
        .await;
        let tls = start_variant("TLS", "tls", retained_router("tls"), |listener, router| {
            camber::http::serve_background_tls(listener, router, Arc::clone(&tls_config))
                .expect("owned server requires a Tokio runtime")
        })
        .await;
        let host = start_variant(
            "host",
            "host",
            retained_router("host"),
            |listener, router| {
                camber::http::serve_background_hosts(listener, host_dispatch(router))
                    .expect("owned server requires a Tokio runtime")
            },
        )
        .await;
        let host_tls = start_variant(
            "host-plus-TLS",
            "host-tls",
            retained_router("host-tls"),
            |listener, router| {
                camber::http::serve_background_hosts_tls(
                    listener,
                    host_dispatch(router),
                    tls_config,
                )
                .expect("owned server requires a Tokio runtime")
            },
        )
        .await;
        let addrs = [
            plain.proof.addr,
            tls.proof.addr,
            host.proof.addr,
            host_tls.proof.addr,
        ];
        let clients = spawn_variant_clients(addrs, connector);
        Self::from_started([plain, tls, host, host_tls], clients)
    }

    fn from_started(started: [StartedVariant; 4], clients: [VariantClient; 4]) -> Self {
        let [plain, tls, host, host_tls] = started;
        Self {
            proofs: [plain.proof, tls.proof, host.proof, host_tls.proof],
            handles: [plain.handle, tls.handle, host.handle, host_tls.handle],
            entered: [plain.entered, tls.entered, host.entered, host_tls.entered],
            releases: [plain.release, tls.release, host.release, host_tls.release],
            clients,
        }
    }
}

async fn wait_for_all(proofs: &VariantProofs, checkpoint: LifecycleCheckpoint) {
    let [plain, tls, host, host_tls] = proofs;
    tokio::join!(
        wait_for_checkpoint(&plain.lifecycle, checkpoint),
        wait_for_checkpoint(&tls.lifecycle, checkpoint),
        wait_for_checkpoint(&host.lifecycle, checkpoint),
        wait_for_checkpoint(&host_tls.lifecycle, checkpoint),
    );
}

async fn await_retained_requests(
    proofs: &VariantProofs,
    entered: [tokio::sync::oneshot::Receiver<()>; 4],
    clients: &mut [VariantClient; 4],
) {
    let [plain, tls, host, host_tls] = entered;
    let [plain_client, tls_client, host_client, host_tls_client] = clients;
    let waits = tokio::join!(
        await_retained_request(proofs[0].name, plain, plain_client),
        await_retained_request(proofs[1].name, tls, tls_client),
        await_retained_request(proofs[2].name, host, host_client),
        await_retained_request(proofs[3].name, host_tls, host_tls_client),
    );
    for result in [waits.0, waits.1, waits.2, waits.3] {
        result.unwrap();
    }
    for proof in proofs {
        assert!(proof.ownership.upgrade().is_some());
    }
}

async fn await_retained_request(
    variant: &str,
    entered: tokio::sync::oneshot::Receiver<()>,
    client: &mut VariantClient,
) -> Result<(), Box<str>> {
    let dispatch = async {
        tokio::select! {
            entered = entered => entered.map_err(|_| {
                format!("{variant} server exited before dispatch").into_boxed_str()
            }),
            response = client => Err(format!(
                "{variant} client exited before dispatch: {response:?}"
            ).into_boxed_str()),
        }
    };
    tokio::time::timeout(EVENT_TIMEOUT, dispatch)
        .await
        .map_err(|_| format!("{variant} did not dispatch its retained request").into_boxed_str())?
}

fn pin_joins(handles: [ServerHandle; 4]) -> [VariantJoin; 4] {
    handles.map(|handle| Box::pin(handle.into_future()))
}

async fn stop_admission(proofs: &VariantProofs) {
    wait_for_all(proofs, LifecycleCheckpoint::SupervisorSelectedRuntime).await;
    for proof in proofs {
        proof
            .lifecycle
            .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
            .unwrap();
        proof
            .lifecycle
            .release(LifecycleCheckpoint::SupervisorSelectedRuntime)
            .unwrap();
    }
    wait_for_all(proofs, LifecycleCheckpoint::BeforeSupervisorSelect).await;
    let [plain, tls, host, host_tls] = proofs;
    tokio::join!(
        common::assert_admission_closed(plain.addr, EVENT_TIMEOUT),
        common::assert_admission_closed(tls.addr, EVENT_TIMEOUT),
        common::assert_admission_closed(host.addr, EVENT_TIMEOUT),
        common::assert_admission_closed(host_tls.addr, EVENT_TIMEOUT),
    );
    for proof in proofs {
        proof
            .lifecycle
            .release(LifecycleCheckpoint::BeforeSupervisorSelect)
            .unwrap();
    }
}

fn assert_joins_pending(proofs: &VariantProofs, joins: &mut [VariantJoin; 4]) {
    for (proof, join) in proofs.iter().zip(joins) {
        assert!(
            join.as_mut().now_or_never().is_none(),
            "{} handle completed while request ownership was retained",
            proof.name
        );
    }
}

fn release_requests(releases: [tokio::sync::oneshot::Sender<()>; 4]) {
    for release in releases {
        release.send(()).unwrap();
    }
}

async fn assert_responses(proofs: &VariantProofs, clients: [VariantClient; 4]) {
    let [plain, tls, host, host_tls] = clients;
    let responses = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(plain, tls, host, host_tls)
    })
    .await
    .expect("retained requests did not complete after release");
    for (proof, response) in proofs
        .iter()
        .zip([responses.0, responses.1, responses.2, responses.3])
    {
        assert_eq!(
            response.unwrap().unwrap(),
            (200, proof.expected_body.to_owned())
        );
    }
}

async fn assert_joined_and_released(proofs: &VariantProofs, joins: [VariantJoin; 4]) {
    let [plain, tls, host, host_tls] = joins;
    let results = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(plain, tls, host, host_tls)
    })
    .await
    .expect("background variants did not join after retained requests completed");
    for (proof, result) in proofs
        .iter()
        .zip([results.0, results.1, results.2, results.3])
    {
        assert_flat_ok(result, proof.name);
    }
    for proof in proofs {
        assert!(proof.ownership.upgrade().is_none());
    }
}

/// 1.T8: Every background constructor stops admission, drains retained work,
/// releases request ownership, and exposes the same flat successful result.
#[camber::test]
async fn all_background_variants_share_internal_lifecycle_behavior() {
    let BackgroundVariants {
        proofs,
        handles,
        entered,
        releases,
        mut clients,
    } = BackgroundVariants::start().await;
    await_retained_requests(&proofs, entered, &mut clients).await;
    let mut joins = pin_joins(handles);
    camber::runtime::request_shutdown();
    stop_admission(&proofs).await;
    assert_joins_pending(&proofs, &mut joins);
    release_requests(releases);
    assert_responses(&proofs, clients).await;
    assert_joined_and_released(&proofs, joins).await;
}
