use crate::runtime_support as common;

use camber::http::mock::{InboundTerminal, TransferObservation, TransferOwnerController};
use camber::http::{Request, Router, StreamResponse, TransferBudget};
use camber::{RuntimeError, runtime};
use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

const TRUNCATED_PREFIX: &[u8] = b"known-upstream-prefix";
const ADVERTISED_BODY_LENGTH: usize = TRUNCATED_PREFIX.len() + 17;
/// The payload the three incremental chunks add up to.
const INCREMENTAL_BYTES: usize = 21;
/// The payload maximum the buffered row's router names.
///
/// Above what its producer sends: the claim is which policy the unnamed
/// `with_buffer` spelling inherited, not a crossing.
const BUFFERED_MAX_BYTES: usize = 64;

/// Wait until this listener's download owner fixed a terminal and released.
///
/// The release alone is [`crate::stream_support::released_download`]; this is
/// that wait with the terminal added, because the rows below read both facts and
/// a wait that settled on one would race the turn that fixes the other.
fn settled_download(controller: &TransferOwnerController, row: &str) -> TransferObservation {
    crate::stream_support::download_observed(controller, row, "settled", |observed| {
        observed.download.terminal.is_some() && observed.download.releases >= 1
    })
}

enum StreamCompletion {
    BodyError(Box<str>),
    Clean(Box<[u8]>),
}

fn spawn_content_length_truncating_upstream() -> (
    SocketAddr,
    mpsc::SyncSender<()>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind raw upstream");
    let address = listener.local_addr().expect("read raw upstream address");
    let (close_tx, close_rx) = mpsc::sync_channel(0);
    let owner = std::thread::spawn(move || serve_truncated_response(listener, close_rx));
    (address, close_tx, owner)
}

fn serve_truncated_response(listener: TcpListener, close_rx: mpsc::Receiver<()>) {
    let (mut stream, _) = listener.accept().expect("accept proxy connection");
    let request_head = read_request_head(&mut stream).expect("read proxy request head");
    let request_head = std::str::from_utf8(&request_head).expect("proxy request head is UTF-8");
    assert!(
        request_head.starts_with("GET /failure HTTP/1.1\r\n"),
        "proxy sent an unexpected request: {request_head:?}"
    );

    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {ADVERTISED_BODY_LENGTH}\r\nConnection: close\r\n\r\n"
    )
    .expect("write truncated upstream response head");
    stream
        .write_all(TRUNCATED_PREFIX)
        .expect("write known upstream prefix");
    stream.flush().expect("flush known upstream prefix");

    close_rx.recv().expect("client releases upstream close");
    // Dropping the socket here truncates the advertised body at a controlled point.
}

fn read_request_head(stream: &mut TcpStream) -> io::Result<Box<[u8]>> {
    const LIMIT: usize = 16 * 1024;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() == LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "upstream request head exceeded fixture limit",
            ));
        }
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    Ok(head.into_boxed_slice())
}

async fn observe_h2_completion(
    proxy_addr: SocketAddr,
    close_tx: mpsc::SyncSender<()>,
) -> StreamCompletion {
    let tcp = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect HTTP/2 client to streaming proxy");
    let (mut client, connection) = h2::client::handshake(tcp)
        .await
        .expect("complete HTTP/2 client handshake");
    let connection_owner = tokio::spawn(connection);
    let request = ::http::Request::get(format!("http://{proxy_addr}/api/failure"))
        .version(::http::Version::HTTP_2)
        .body(())
        .expect("build HTTP/2 streaming proxy request");
    client = client
        .ready()
        .await
        .expect("HTTP/2 request sender becomes ready");
    let (response, _) = client
        .send_request(request, true)
        .expect("send HTTP/2 streaming proxy request");
    let response = response.await.expect("receive proxied response head");

    assert_eq!(response.version(), ::http::Version::HTTP_2);
    assert_eq!(response.status(), 200);
    assert!(
        !response
            .headers()
            .contains_key(::http::header::TRANSFER_ENCODING),
        "HTTP/2 response must not use HTTP/1 transfer coding"
    );
    let content_lengths = response
        .headers()
        .get_all(::http::header::CONTENT_LENGTH)
        .iter()
        .collect::<Vec<_>>();
    assert_eq!(content_lengths.len(), 1, "expected one Content-Length");
    assert_eq!(
        content_lengths[0]
            .to_str()
            .expect("Content-Length is visible ASCII")
            .parse::<usize>()
            .expect("Content-Length is numeric"),
        ADVERTISED_BODY_LENGTH
    );

    let mut body = response.into_body();
    let mut prefix = Vec::with_capacity(TRUNCATED_PREFIX.len());
    while prefix.len() < TRUNCATED_PREFIX.len() {
        let data = body
            .data()
            .await
            .expect("body remains open until the known prefix arrives")
            .expect("known prefix arrives without a downstream body error");
        prefix.extend_from_slice(&data);
    }
    assert_eq!(prefix.as_slice(), TRUNCATED_PREFIX);

    close_tx
        .send(())
        .expect("release synchronized upstream close");
    let completion = match body.data().await {
        Some(Err(error)) => StreamCompletion::BodyError(error.to_string().into_boxed_str()),
        Some(Ok(bytes)) => StreamCompletion::Clean(bytes.to_vec().into_boxed_slice()),
        None => StreamCompletion::Clean(Box::new([])),
    };
    drop(client);
    connection_owner.abort();
    completion
}

#[test]
fn stream_failure_is_observable_to_client_or_owner() {
    // This public-boundary proof covers protocol-observable source failure: an
    // upstream closes before its declared Content-Length. It does not claim
    // that an unframed producer failure can be distinguished from clean EOF.
    let (upstream_addr, close_tx, upstream_owner) = spawn_content_length_truncating_upstream();
    let owner_result = common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.proxy_stream("/api", &format!("http://{upstream_addr}"));
            let port = crate::http::reserve_transfer_owner();
            let server = port.serve(router);
            let completion = common::block_on(observe_h2_completion(server.addr(), close_tx));
            // 11.T2 and 11.T4: the truncation the peer or the owner reports is
            // a terminal one production download owner fixed, and that owner
            // released its upstream source once rather than leaving it held.
            let observed = settled_download(server.controller(), "the truncated upstream");
            assert_eq!(
                observed.download.terminal,
                Some(InboundTerminal::SourceFailure),
                "the upstream's own failure is the download owner's terminal: {observed:?}"
            );
            assert_eq!(
                observed.download.releases, 1,
                "the download owner released its upstream source once: {observed:?}"
            );
            runtime::request_shutdown();
            completion
        });

    upstream_owner.join().expect("raw upstream owner joins");
    match owner_result {
        Err(RuntimeError::Http(owner_error)) => assert!(!owner_error.is_empty()),
        Err(RuntimeError::Io(owner_error)) => assert!(!owner_error.to_string().is_empty()),
        Err(owner_error) => panic!("unrelated proxy owner failure: {owner_error}"),
        Ok(StreamCompletion::BodyError(error)) => assert!(!error.is_empty()),
        Ok(StreamCompletion::Clean(bytes)) => panic!(
            "proxy reported clean body completion after Content-Length truncation; trailing bytes: {bytes:?}"
        ),
    }
}

#[test]
fn stream_response_sends_chunks_incrementally() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let (first_sent_tx, first_sent_rx) = mpsc::sync_channel(1);
            let (release_tx, release_rx) = mpsc::sync_channel(1);
            let release_rx = Arc::new(Mutex::new(release_rx));
            // 11.T1: the incremental delivery below runs under a named payload
            // maximum, so what the peer receives one chunk at a time is what one
            // production download owner admitted and accounted for.
            let budget = TransferBudget::unbounded()
                .with_max_bytes(INCREMENTAL_BYTES)
                .expect("the incremental payload maximum is accepted");
            let mut router = Router::new();
            router.get_stream("/stream", move |_req: &Request| {
                let first_sent_tx = first_sent_tx.clone();
                let release_rx = Arc::clone(&release_rx);
                Box::pin(async move {
                    let (stream_resp, sender) = StreamResponse::with_budget(200, 4, budget)
                        .expect("a positive stream capacity is accepted");

                    tokio::spawn(async move {
                        sender.send("chunk-0").await.unwrap();
                        first_sent_tx.send(()).unwrap();
                        tokio::task::block_in_place(|| {
                            release_rx
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(2))
                                .unwrap();
                        });
                        sender.send("chunk-1").await.unwrap();
                        sender.send("chunk-2").await.unwrap();
                    });

                    stream_resp
                })
            });

            let port = crate::http::reserve_transfer_owner();
            let server = port.serve(router);
            let addr = server.addr();

            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();

            assert_chunks_arrive_one_at_a_time(stream, &first_sent_rx, &release_tx);
            assert_budgeted_download(server.controller(), "incremental chunks", INCREMENTAL_BYTES);

            runtime::request_shutdown();
        })
        .unwrap();
}

/// Each chunk reaches the peer as its own write, the second only after release.
///
/// The producer holds between the first chunk and the rest, so a response that
/// buffered its body could not answer the first read at all.
fn assert_chunks_arrive_one_at_a_time(
    stream: TcpStream,
    first_sent: &mpsc::Receiver<()>,
    release: &mpsc::SyncSender<()>,
) {
    let mut reader = BufReader::new(stream);
    let (status, headers) = crate::wire::read_response_head(&mut reader);
    assert_eq!(status, 200);
    assert!(headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked")
    }));
    first_sent.recv_timeout(Duration::from_secs(2)).unwrap();
    let first_chunk = crate::wire::read_chunk(&mut reader, 1024)
        .expect("decode first chunk")
        .expect("first chunk");
    assert_eq!(first_chunk.as_ref(), b"chunk-0");
    release.send(()).unwrap();
    for expected in [b"chunk-1".as_slice(), b"chunk-2".as_slice()] {
        let chunk = crate::wire::read_chunk(&mut reader, 1024)
            .expect("decode released chunk")
            .expect("released chunk");
        assert_eq!(chunk.as_ref(), expected);
    }
    assert!(
        crate::wire::read_chunk(&mut reader, 1024)
            .expect("decode terminal chunk and trailers")
            .is_none()
    );
}

/// The maximum this row named reached its owner, which accounted under it.
///
/// Both budgeted rows end here: one names the maximum on the response and one
/// inherits it from the router, and what each proves about the owner that
/// carried it is the same four facts.
fn assert_budgeted_download(controller: &TransferOwnerController, row: &str, maximum: usize) {
    let observed = settled_download(controller, row);
    assert_eq!(
        observed.download.max_bytes,
        Some(maximum),
        "{row}: the maximum production froze is the one this row named: {observed:?}"
    );
    assert_eq!(
        observed.download.admitted_bytes, INCREMENTAL_BYTES,
        "{row}: every chunk the peer received was admitted by the owner: {observed:?}"
    );
    assert_eq!(
        observed.download.crossings_released, 0,
        "{row}: a payload under its maximum releases nothing"
    );
    assert_eq!(
        observed.download.terminal,
        Some(InboundTerminal::ResponseHead),
        "{row}: the producer's own end is the terminal: {observed:?}"
    );
}

/// 14.T1 provisional head projection.
///
/// A stream route has two owners of its head, and this row reads both. The
/// producer names what it commits, and a chain that ran over the provisional
/// head it was shown before that commit has its own metadata carried onto it —
/// every value under a name the producer left alone, and nothing at all under
/// a name the producer claimed.
#[test]
fn stream_response_with_custom_headers() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(|req: &Request, next: camber::http::Next| {
                let entered = next.call(req);
                async move {
                    entered
                        .await
                        .with_header("X-Projected", "applied")
                        .with_header("Set-Cookie", "first=1")
                        .with_header("Set-Cookie", "second=2")
                        // The name the producer commits for itself: what the
                        // chain states over it must not displace it.
                        .with_header("X-Custom", "projected")
                }
            });
            router.get_stream("/stream", |_req: &Request| {
                Box::pin(async {
                    let (stream_resp, sender) = StreamResponse::new(200);
                    let stream_resp = stream_resp.with_header("X-Custom", "value");

                    tokio::spawn(async move {
                        sender.send("hello").await.unwrap();
                    });

                    stream_resp
                })
            });

            let addr = common::spawn_server(router);

            let response =
                crate::http::request(addr, "GET", "/stream", &[], &[], Duration::from_secs(5))
                    .unwrap();
            assert_eq!(response.status, 200);
            // Every value under the name, not the first: a merge that appended
            // beside the producer's own header rather than standing aside reads
            // identically through a first-value lookup.
            assert_eq!(
                response.header_values("x-custom").as_ref(),
                ["value"],
                "the chain's value displaced the one the producer committed",
            );
            assert_eq!(
                response.header("x-projected"),
                Some("applied"),
                "the metadata the chain stated never reached the committed head",
            );
            assert_eq!(
                response.header_values("set-cookie").as_ref(),
                ["first=1", "second=2"],
                "a chain that stated two values under one name lost one",
            );
            assert_eq!(response.body.as_ref(), b"hello");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_response_client_disconnect_drops_sender() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let (disconnected_tx, disconnected_rx) = mpsc::sync_channel(1);

            let mut router = Router::new();
            router.get_stream("/stream", move |_req: &Request| {
                let disconnected_tx = disconnected_tx.clone();
                Box::pin(async move {
                    let (stream_resp, sender) = StreamResponse::new(200);

                    tokio::spawn(async move {
                        loop {
                            if sender.send("tick").await.is_err() {
                                disconnected_tx.send(()).unwrap();
                                return;
                            }
                        }
                    });

                    stream_resp
                })
            });

            let port = crate::http::reserve_transfer_owner();
            let server = port.serve(router);
            let addr = server.addr();

            // Connect, read first chunk, then drop
            {
                let mut stream = TcpStream::connect(addr).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                write!(
                    stream,
                    "GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                stream.flush().unwrap();

                let mut reader = BufReader::new(stream);
                let (status, _) = crate::wire::read_response_head(&mut reader);
                assert_eq!(status, 200);
                let chunk = crate::wire::read_chunk(&mut reader, 1024)
                    .expect("decode bounded stream chunk")
                    .expect("stream remained open for first chunk");
                assert_eq!(chunk.as_ref(), b"tick");
            }
            disconnected_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("sender observed client disconnect");

            // 11.T2: the sender the producer lost is the source one production
            // download owner released, and it released it exactly once.
            let row = "a departed peer";
            let observed = crate::stream_support::released_download(server.controller(), row);
            assert_eq!(
                observed.download.releases, 1,
                "{row}: the owner released its source and producer once: {observed:?}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_response_empty_body() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.get_stream("/empty", |_req: &Request| {
                Box::pin(async {
                    let (stream_resp, _sender) = StreamResponse::new(204);
                    // sender dropped immediately — empty body
                    stream_resp
                })
            });
            // 11.T1: a producer whose one frame carried no payload. The peer sees
            // the same empty body either way, so the frame that was polled and the
            // bytes it did not cost are read from the production owner.
            router.get_stream("/empty-frame", |_req: &Request| {
                Box::pin(async {
                    let (stream_resp, sender) = StreamResponse::new(200);
                    tokio::spawn(async move {
                        let _departed = sender.send(&b""[..]).await;
                    });
                    stream_resp
                })
            });

            let port = crate::http::reserve_transfer_owner();
            let server = port.serve(router);
            let addr = server.addr();

            let mut stream = crate::http::connect(addr).expect("connect to empty stream route");
            write!(
                stream,
                "GET /empty HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .expect("write empty stream request");
            stream.flush().expect("flush empty stream request");

            let mut reader = BufReader::new(stream);
            let (status, _) = crate::wire::read_response_head(&mut reader);
            assert_eq!(status, 204);
            let bytes_after_head = crate::wire::read_to_eof_bounded(&mut reader, 1024)
                .expect("read empty stream through connection close");
            assert_eq!(bytes_after_head.as_ref(), b"");

            assert_empty_frame_row(addr, server.controller());

            runtime::request_shutdown();
        })
        .unwrap();
}

/// The payload-free frame this file's second empty route produces.
///
/// The peer's view is the same empty body a dropped sender gives, so what tells
/// the two apart is the owner's own record: it polled a frame, charged it
/// nothing, released nothing, and ended on the source's own end.
fn assert_empty_frame_row(addr: SocketAddr, controller: &TransferOwnerController) {
    let row = "an empty frame";
    let answered = crate::http::request(
        addr,
        "GET",
        "/empty-frame",
        &[],
        &[],
        Duration::from_secs(5),
    )
    .expect("read the empty-frame stream");
    assert_eq!(answered.status, 200, "{row}: the feed committed its status");
    assert_eq!(
        answered.body.as_ref(),
        b"",
        "{row}: no payload reaches the peer"
    );
    let observed = settled_download(controller, row);
    assert!(
        observed.download.frames_polled >= 1,
        "{row}: the empty frame was polled out of the source: {observed:?}"
    );
    assert_eq!(
        observed.download.admitted_bytes, 0,
        "{row}: an empty frame costs no payload bytes: {observed:?}"
    );
    assert_eq!(
        observed.download.crossings_released, 0,
        "{row}: nothing was released instead of delivered"
    );
    assert_eq!(
        observed.download.terminal,
        Some(InboundTerminal::ResponseHead),
        "{row}: the source's own end is the terminal: {observed:?}"
    );
}

#[test]
fn stream_response_with_buffer_rejects_zero_capacity() {
    assert_zero_capacity_refused("with_buffer", StreamResponse::with_buffer(200, 0));
    // 11.T1: the budgeted spelling validates the same capacity, and it validates
    // it before the budget it also names — a stream that could name a maximum and
    // no channel to carry it would have a bound over nothing.
    assert_zero_capacity_refused(
        "with_budget",
        StreamResponse::with_budget(200, 0, TransferBudget::unbounded()),
    );
    let (_response, _sender) = StreamResponse::with_budget(200, 1, TransferBudget::unbounded())
        .expect("with_budget accepts the smallest positive capacity");
}

/// Both public streaming spellings refuse a zero capacity the same way.
fn assert_zero_capacity_refused<T>(spelling: &str, result: Result<T, RuntimeError>) {
    match result {
        Err(RuntimeError::InvalidArgument(msg)) => {
            assert!(
                msg.contains("capacity"),
                "{spelling}: error should mention capacity, got: {msg}"
            );
        }
        Err(other) => panic!("{spelling}: expected InvalidArgument, got: {other}"),
        Ok(_) => panic!("{spelling}: expected error for zero capacity"),
    }
}

#[test]
fn stream_response_with_buffer_preserves_streaming_behavior() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.get_stream("/buffered", |_req: &Request| {
                Box::pin(async {
                    let (stream_resp, sender) = StreamResponse::with_buffer(200, 1).unwrap();

                    tokio::spawn(async move {
                        for i in 0..3 {
                            sender.send(format!("chunk-{i}")).await.unwrap();
                        }
                    });

                    stream_resp
                })
            });

            // 11.T1: the unnamed `with_buffer` spelling names no transfer policy
            // of its own, so the router's download policy is what its owner must
            // freeze. A spelling that widened instead of inheriting would freeze
            // no maximum at all.
            let inherited = TransferBudget::unbounded()
                .with_max_bytes(BUFFERED_MAX_BYTES)
                .expect("the router's download maximum is accepted");
            let port = crate::http::reserve_transfer_owner();
            let server = port.serve(router.download_budget(inherited));
            let addr = server.addr();

            let response =
                crate::http::request(addr, "GET", "/buffered", &[], &[], Duration::from_secs(5))
                    .unwrap();
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"chunk-0chunk-1chunk-2");

            let row = "a backpressured buffer";
            assert_budgeted_download(server.controller(), row, BUFFERED_MAX_BYTES);
            assert!(
                server.controller().observed().download.frames_polled >= 3,
                "{row}: each chunk crossed the owner as its own frame"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}
