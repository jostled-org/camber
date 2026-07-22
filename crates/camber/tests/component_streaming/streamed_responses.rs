use crate::runtime_support as common;

use camber::http::{Request, Router, StreamResponse};
use camber::{RuntimeError, runtime};
use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

const TRUNCATED_PREFIX: &[u8] = b"known-upstream-prefix";
const ADVERTISED_BODY_LENGTH: usize = TRUNCATED_PREFIX.len() + 17;

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
            let proxy_addr = common::spawn_server(router);
            let completion = common::block_on(observe_h2_completion(proxy_addr, close_tx));
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
            let mut router = Router::new();
            router.get_stream("/stream", move |_req: &Request| {
                let first_sent_tx = first_sent_tx.clone();
                let release_rx = Arc::clone(&release_rx);
                Box::pin(async move {
                    let (stream_resp, sender) = StreamResponse::new(200);

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

            let addr = common::spawn_server(router);

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
            let (status, headers) = crate::wire::read_response_head(&mut reader);
            assert_eq!(status, 200);
            assert!(headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("transfer-encoding")
                    && value.eq_ignore_ascii_case("chunked")
            }));
            first_sent_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let first_chunk = crate::wire::read_chunk(&mut reader, 1024)
                .expect("decode first chunk")
                .expect("first chunk");
            assert_eq!(first_chunk.as_ref(), b"chunk-0");
            release_tx.send(()).unwrap();
            let second_chunk = crate::wire::read_chunk(&mut reader, 1024)
                .expect("decode second chunk")
                .expect("second chunk");
            assert_eq!(second_chunk.as_ref(), b"chunk-1");
            let third_chunk = crate::wire::read_chunk(&mut reader, 1024)
                .expect("decode third chunk")
                .expect("third chunk");
            assert_eq!(third_chunk.as_ref(), b"chunk-2");
            assert!(
                crate::wire::read_chunk(&mut reader, 1024)
                    .expect("decode terminal chunk and trailers")
                    .is_none()
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_response_with_custom_headers() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
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
            assert_eq!(response.header("x-custom"), Some("value"));
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

            let addr = common::spawn_server(router);

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

            let addr = common::spawn_server(router);

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

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_response_with_buffer_rejects_zero_capacity() {
    let result = StreamResponse::with_buffer(200, 0);
    match result {
        Err(RuntimeError::InvalidArgument(msg)) => {
            assert!(
                msg.contains("capacity"),
                "error should mention capacity, got: {msg}"
            );
        }
        Err(other) => panic!("expected InvalidArgument, got: {other}"),
        Ok(_) => panic!("expected error for zero capacity"),
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

            let addr = common::spawn_server(router);

            let response =
                crate::http::request(addr, "GET", "/buffered", &[], &[], Duration::from_secs(5))
                    .unwrap();
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"chunk-0chunk-1chunk-2");

            runtime::request_shutdown();
        })
        .unwrap();
}
