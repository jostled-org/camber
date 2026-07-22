use crate::common;

use bytes::Bytes;
use camber::http::Router;
use camber::runtime;
use futures_util::FutureExt;
use hyper::body::{Body, Frame};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

const DOWNSTREAM_WINDOW_BYTES: usize = 1024;
const UPSTREAM_FRAME_BYTES: usize = 64 * 1024;
const UPSTREAM_FRAME_LIMIT: usize = 256;
const FIRST_BOUNDARY: &[u8] = b"[first-upstream-frame]";
const FINAL_BOUNDARY: &[u8] = b"[final-upstream-frame]";
const FRAME_BOUNDARY: &[u8] = b"[ordered-frame:]";
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
enum BodyPollCheckpoint {
    Data(usize),
    End(usize),
}

struct BodyControl {
    end_at: AtomicUsize,
    poll_count: AtomicUsize,
    complete: AtomicBool,
}

impl BodyControl {
    fn new() -> Self {
        Self {
            end_at: AtomicUsize::new(UPSTREAM_FRAME_LIMIT),
            poll_count: AtomicUsize::new(0),
            complete: AtomicBool::new(false),
        }
    }
}

struct CheckpointBody {
    control: Arc<BodyControl>,
    checkpoints: tokio::sync::mpsc::UnboundedSender<BodyPollCheckpoint>,
    next_frame: usize,
}

impl Body for CheckpointBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let end_at = self.control.end_at.load(Ordering::Acquire);
        self.control.poll_count.fetch_add(1, Ordering::AcqRel);

        match self.next_frame == end_at {
            true => {
                self.control.complete.store(true, Ordering::Release);
                self.checkpoints
                    .send(BodyPollCheckpoint::End(end_at))
                    .expect("body checkpoint receiver remains live");
                Poll::Ready(None)
            }
            false => {
                let index = self.next_frame;
                self.next_frame += 1;
                self.checkpoints
                    .send(BodyPollCheckpoint::Data(index))
                    .expect("body checkpoint receiver remains live");
                Poll::Ready(Some(Ok(Frame::data(ordered_frame(index, end_at)))))
            }
        }
    }
}

#[derive(Debug)]
struct ConnectionPollReport {
    body_polls_before: usize,
    body_polls_after: usize,
    connection_finished: bool,
}

struct GatedConnection<F> {
    connection: Pin<Box<F>>,
    permits: tokio::sync::mpsc::UnboundedReceiver<()>,
    reports: tokio::sync::mpsc::UnboundedSender<ConnectionPollReport>,
    body_poll_count: Arc<BodyControl>,
    continuous: Arc<AtomicBool>,
}

impl<F> Future for GatedConnection<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.continuous.load(Ordering::Acquire) {
            return self.connection.as_mut().poll(context);
        }

        match self.permits.poll_recv(context) {
            Poll::Ready(Some(())) => {}
            Poll::Ready(None) | Poll::Pending => return Poll::Pending,
        }

        let body_polls_before = self.body_poll_count.poll_count.load(Ordering::Acquire);
        let result = self.connection.as_mut().poll(context);
        let report = ConnectionPollReport {
            body_polls_before,
            body_polls_after: self.body_poll_count.poll_count.load(Ordering::Acquire),
            connection_finished: result.is_ready(),
        };
        self.reports
            .send(report)
            .expect("connection poll report receiver remains live");
        if result.is_pending() {
            // Re-poll only the gate so the permit receiver registers this task's waker.
            context.waker().wake_by_ref();
        }
        result
    }
}

struct ConnectionController {
    permits: tokio::sync::mpsc::UnboundedSender<()>,
    reports: tokio::sync::mpsc::UnboundedReceiver<ConnectionPollReport>,
    continuous: Arc<AtomicBool>,
}

impl ConnectionController {
    async fn poll_once(&mut self) -> ConnectionPollReport {
        self.permits
            .send(())
            .expect("gated upstream connection remains live");
        self.reports
            .recv()
            .await
            .expect("gated connection reports every permitted poll")
    }

    fn run_continuously(&self) {
        self.continuous.store(true, Ordering::Release);
        self.permits
            .send(())
            .expect("gated upstream connection remains live");
    }
}

async fn event<F>(name: &str, future: F) -> F::Output
where
    F: Future,
{
    tokio::time::timeout(EVENT_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {name}"))
}

fn ordered_frame(index: usize, end_at: usize) -> Bytes {
    match (index, index + 1 == end_at) {
        (0, _) => padded_frame(FIRST_BOUNDARY, index),
        (_, true) => Bytes::from_static(FINAL_BOUNDARY),
        _ => padded_frame(FRAME_BOUNDARY, index),
    }
}

fn padded_frame(boundary: &[u8], index: usize) -> Bytes {
    let mut bytes = Vec::with_capacity(UPSTREAM_FRAME_BYTES);
    bytes.extend_from_slice(boundary);
    bytes.extend_from_slice(&(index as u64).to_be_bytes());
    bytes.resize(UPSTREAM_FRAME_BYTES, (index % 251) as u8);
    bytes.into()
}

fn expected_body(frame_count: usize) -> Vec<u8> {
    (0..frame_count)
        .flat_map(|index| ordered_frame(index, frame_count))
        .collect()
}

async fn spawn_gated_upstream(
    stream: tokio::net::TcpStream,
    body_control: Arc<BodyControl>,
    checkpoints: tokio::sync::mpsc::UnboundedSender<BodyPollCheckpoint>,
) -> (
    ConnectionController,
    tokio::task::JoinHandle<Result<(), hyper::Error>>,
) {
    let body = CheckpointBody {
        control: Arc::clone(&body_control),
        checkpoints,
        next_frame: 0,
    };
    let body = Arc::new(std::sync::Mutex::new(Some(body)));
    let service = service_fn(move |_request| {
        let body = body
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("the upstream serves one request");
        async move { Ok::<_, Infallible>(hyper::Response::new(body)) }
    });
    let connection = http1::Builder::new().serve_connection(TokioIo::new(stream), service);
    let (permit_tx, permit_rx) = tokio::sync::mpsc::unbounded_channel();
    let (report_tx, report_rx) = tokio::sync::mpsc::unbounded_channel();
    let continuous = Arc::new(AtomicBool::new(false));
    let gated = GatedConnection {
        connection: Box::pin(connection),
        permits: permit_rx,
        reports: report_tx,
        body_poll_count: body_control,
        continuous: Arc::clone(&continuous),
    };
    let task = tokio::spawn(gated);
    (
        ConnectionController {
            permits: permit_tx,
            reports: report_rx,
            continuous,
        },
        task,
    )
}

async fn response_head_while_driving(
    response: h2::client::ResponseFuture,
    upstream: &mut ConnectionController,
) -> ::http::Response<h2::RecvStream> {
    let mut response = Box::pin(response);
    loop {
        if let Some(result) = response.as_mut().now_or_never() {
            return result.expect("proxy returns an HTTP/2 response head");
        }
        let report = upstream.poll_once().await;
        assert!(
            !report.connection_finished,
            "upstream connection ended early"
        );
    }
}

async fn data_while_driving(
    body: &mut h2::RecvStream,
    upstream: &mut ConnectionController,
) -> Option<Result<Bytes, h2::Error>> {
    let mut data = Box::pin(body.data());
    loop {
        if let Some(result) = data.as_mut().now_or_never() {
            return result;
        }
        let report = upstream.poll_once().await;
        assert!(
            !report.connection_finished,
            "upstream connection ended early"
        );
    }
}

fn drain_data_checkpoints(
    checkpoints: &mut tokio::sync::mpsc::UnboundedReceiver<BodyPollCheckpoint>,
) -> Vec<usize> {
    std::iter::from_fn(|| checkpoints.try_recv().ok())
        .map(|checkpoint| match checkpoint {
            BodyPollCheckpoint::Data(index) => index,
            BodyPollCheckpoint::End(count) => {
                panic!("upstream body ended at {count} frames before flow-control release")
            }
        })
        .collect()
}

#[camber::test]
async fn adversarial_proxy_stream_obeys_downstream_http2_flow_control() {
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test-owned upstream listener");
    let upstream_addr = upstream_listener
        .local_addr()
        .expect("read test-owned upstream address");
    let body_control = Arc::new(BodyControl::new());
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut proxy = Router::new();
    proxy.proxy_stream("/api", &format!("http://{upstream_addr}"));
    let proxy_addr = common::spawn_server(proxy);

    let tcp = event(
        "the local proxy connection",
        tokio::net::TcpStream::connect(proxy_addr),
    )
    .await
    .expect("connect local HTTP/2 client");
    let mut h2_builder = h2::client::Builder::new();
    h2_builder
        .initial_window_size(DOWNSTREAM_WINDOW_BYTES as u32)
        .initial_connection_window_size(DOWNSTREAM_WINDOW_BYTES as u32);
    let (mut client, downstream_connection) = event(
        "the local HTTP/2 handshake",
        h2_builder.handshake::<_, Bytes>(tcp),
    )
    .await
    .expect("complete local HTTP/2 handshake");
    let downstream_connection = tokio::spawn(downstream_connection);

    let request = ::http::Request::get(format!("http://{proxy_addr}/api/flow"))
        .version(::http::Version::HTTP_2)
        .body(())
        .expect("build HTTP/2 flow-control request");
    client = event("HTTP/2 request readiness", client.ready())
        .await
        .expect("HTTP/2 request sender becomes ready");
    let (response, _) = client
        .send_request(request, true)
        .expect("send HTTP/2 flow-control request");

    let (upstream_stream, _) = event(
        "the proxy's upstream connection",
        upstream_listener.accept(),
    )
    .await
    .expect("accept the proxy's upstream connection");
    let (mut upstream, upstream_task) =
        spawn_gated_upstream(upstream_stream, Arc::clone(&body_control), checkpoint_tx).await;
    let response = event(
        "the proxied response head",
        response_head_while_driving(response, &mut upstream),
    )
    .await;
    assert_eq!(response.status(), 200);

    let mut body = response.into_body();
    let first = event(
        "the first decoded downstream DATA frame",
        data_while_driving(&mut body, &mut upstream),
    )
    .await
    .expect("stream contains a first DATA frame")
    .expect("first DATA frame decodes successfully");
    assert!(
        first.starts_with(FIRST_BOUNDARY),
        "first decoded DATA omitted the first-frame boundary"
    );
    assert!(
        !body_control.complete.load(Ordering::Acquire),
        "upstream completed before the first downstream DATA frame was observed"
    );

    let mut actual = first.to_vec();
    while actual.len() < DOWNSTREAM_WINDOW_BYTES {
        let chunk = event(
            "DATA that consumes the configured downstream window",
            data_while_driving(&mut body, &mut upstream),
        )
        .await
        .expect("stream remains open while filling its receive window")
        .expect("window-filling DATA decodes successfully");
        actual.extend_from_slice(&chunk);
    }
    assert_eq!(
        actual.len(),
        DOWNSTREAM_WINDOW_BYTES,
        "decoded DATA must consume the deliberately small receive window exactly"
    );

    let blocked_report = event("upstream transport saturation", async {
        loop {
            let report = upstream.poll_once().await;
            match report.body_polls_after == report.body_polls_before {
                true => return report,
                false => {}
            }
        }
    })
    .await;
    assert!(!blocked_report.connection_finished);
    let emitted_before_release = blocked_report.body_polls_after;
    assert!(
        emitted_before_release > 1,
        "the direct upstream body must enter transport flow before it stalls"
    );
    assert!(
        !body_control.complete.load(Ordering::Acquire),
        "upstream exhausted its safety frame limit instead of being flow-controlled"
    );

    let checkpoints = drain_data_checkpoints(&mut checkpoint_rx);
    assert_eq!(
        checkpoints,
        (0..emitted_before_release).collect::<Vec<_>>(),
        "upstream body poll checkpoints were missing or reordered"
    );
    let withheld_probe = event(
        "the explicit withheld-capacity body-poll probe",
        upstream.poll_once(),
    )
    .await;
    assert_eq!(
        withheld_probe.body_polls_after, withheld_probe.body_polls_before,
        "proxy polled another upstream body frame while downstream capacity was withheld"
    );
    assert!(
        matches!(
            checkpoint_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "a body poll checkpoint appeared while downstream capacity was withheld"
    );

    let final_frame_count = emitted_before_release + 2;
    body_control
        .end_at
        .store(final_frame_count, Ordering::Release);
    body.flow_control()
        .release_capacity(actual.len())
        .expect("release retained downstream HTTP/2 capacity");

    let resumed_report = event("upstream body polling after flow-control release", async {
        loop {
            let report = upstream.poll_once().await;
            match report.body_polls_after > report.body_polls_before {
                true => return report,
                false => {
                    let chunk = body
                        .data()
                        .await
                        .expect("buffered proxy DATA precedes resumed upstream polling")
                        .expect("buffered proxy DATA decodes during flow-control release");
                    actual.extend_from_slice(&chunk);
                    body.flow_control()
                        .release_capacity(chunk.len())
                        .expect("return capacity while draining the proxy transport");
                }
            }
        }
    })
    .await;
    assert!(!resumed_report.connection_finished);
    assert_eq!(
        event("the resumed body poll checkpoint", checkpoint_rx.recv()).await,
        Some(BodyPollCheckpoint::Data(emitted_before_release)),
        "downstream capacity release did not resume at the exact upstream boundary"
    );

    upstream.run_continuously();
    while let Some(chunk) = event("ordered downstream completion", body.data()).await {
        let chunk = chunk.expect("downstream DATA decodes after capacity release");
        actual.extend_from_slice(&chunk);
        body.flow_control()
            .release_capacity(chunk.len())
            .expect("return consumed HTTP/2 capacity");
    }
    assert_eq!(
        event(
            "the terminal upstream body checkpoint",
            checkpoint_rx.recv()
        )
        .await,
        Some(BodyPollCheckpoint::Data(final_frame_count - 1))
    );
    assert_eq!(
        event("upstream body completion", checkpoint_rx.recv()).await,
        Some(BodyPollCheckpoint::End(final_frame_count))
    );

    let expected = expected_body(final_frame_count);
    assert_eq!(
        actual, expected,
        "proxied bytes or frame boundaries changed"
    );
    assert!(actual.starts_with(FIRST_BOUNDARY));
    assert!(actual.ends_with(FINAL_BOUNDARY));

    drop(client);
    downstream_connection.abort();
    upstream_task.abort();
    runtime::request_shutdown();
}
