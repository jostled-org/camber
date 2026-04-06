use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camber::http::{Request, Response, Router, SseWriter};

#[tokio::test(flavor = "multi_thread")]
async fn serve_async_handles_request() {
    let mut router = Router::new();
    router.get("/", |_req: &Request| async { Response::text(200, "hello") });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(camber::http::serve_async(listener, router));

    let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
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
    tokio::spawn(camber::http::serve_async(listener, router));

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
    })
    .await
    .unwrap();

    assert_eq!(events.len(), 3);
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event, &format!("event: message\ndata: data-{i}"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_async_shares_tokio_runtime() {
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let mut router = Router::new();
    let tx_clone = Arc::clone(&tx);
    router.get("/check", move |_req: &Request| {
        // Spawn a tokio task from within the handler — proves we share the runtime
        let sender = tx_clone.lock().unwrap_or_else(|e| e.into_inner()).take();
        async move {
            match sender {
                Some(tx) => {
                    tokio::runtime::Handle::current().spawn(async move {
                        let _ = tx.send(true);
                    });
                    Response::text(200, "spawned")
                }
                None => Response::text(200, "already sent"),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(camber::http::serve_async(listener, router));

    let resp = reqwest::get(format!("http://{addr}/check")).await.unwrap();
    assert_eq!(resp.status(), 200);

    // The handler spawned a tokio task that sends through the oneshot.
    // If serve_async created a separate runtime, this would never resolve.
    let result = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("timeout waiting for spawned task")
        .expect("oneshot channel dropped");
    assert!(result);
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

        match trimmed.is_empty() {
            true if !current.is_empty() => {
                events.push(std::mem::take(&mut current));
                if events.len() >= count {
                    break;
                }
            }
            true => {}
            false => {
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(trimmed);
            }
        }
    }
    events
}
