use crate::runtime_support as common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const GENERATED_RETRY_CASES: u64 = 24;
const RETRY_COUNT_BOUND: NonZeroUsize = NonZeroUsize::new(5).unwrap();

async fn accept_request(listener: &tokio::net::TcpListener) -> tokio::net::TcpStream {
    let (stream, _) = listener.accept().await.unwrap();
    read_request(stream).await
}

async fn read_request(mut stream: tokio::net::TcpStream) -> tokio::net::TcpStream {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        request.push(byte[0]);
    }
    let request_head = std::str::from_utf8(&request).unwrap();
    let content_length = request_head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; content_length];
    stream.read_exact(&mut body).await.unwrap();
    stream
}

async fn transport_failure_then_success(
    listener: tokio::net::TcpListener,
    completion: tokio::sync::oneshot::Receiver<()>,
) -> usize {
    drop(accept_request(&listener).await);
    tokio::select! {
        biased;
        accepted = listener.accept() => {
            let (stream, _) = accepted.unwrap();
            let mut stream = read_request(stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            2
        }
        result = completion => {
            assert!(matches!(result, Ok(())));
            1
        }
    }
}

fn register_method_retry_routes(router: &mut Router, calls: &Arc<[AtomicU32; 7]>) {
    let get_calls = Arc::clone(calls);
    router.get("/method-retry", move |_req: &Request| {
        get_calls[0].fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });

    let head_calls = Arc::clone(calls);
    router.head("/method-retry", move |_req: &Request| {
        head_calls[1].fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });

    let options_calls = Arc::clone(calls);
    router.options("/method-retry", move |_req: &Request| {
        options_calls[2].fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });

    let post_calls = Arc::clone(calls);
    router.post("/method-retry", move |_req: &Request| {
        post_calls[3].fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });

    let put_calls = Arc::clone(calls);
    router.put("/method-retry", move |_req: &Request| {
        put_calls[4].fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });

    let patch_calls = Arc::clone(calls);
    router.patch("/method-retry", move |_req: &Request| {
        patch_calls[5].fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });

    let delete_calls = Arc::clone(calls);
    router.delete("/method-retry", move |_req: &Request| {
        delete_calls[6].fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_method_retry_requires_explicit_policy() {
    const RETRIES: u32 = 2;

    let calls = Arc::new(std::array::from_fn(|_| AtomicU32::new(0)));
    let mut router = Router::new();
    register_method_retry_routes(&mut router, &calls);
    let server = crate::http::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let url = format!("http://{}/method-retry", server.local_addr());
    let client = http::client()
        .retries(RETRIES)
        .backoff(Duration::from_millis(1));

    assert_eq!(client.get(&url).await.unwrap().status(), 503);
    assert_eq!(client.head(&url).await.unwrap().status(), 503);
    assert_eq!(client.options(&url).await.unwrap().status(), 503);
    assert_eq!(client.post(&url, "post").await.unwrap().status(), 503);
    assert_eq!(client.put(&url, "put").await.unwrap().status(), 503);
    assert_eq!(client.patch(&url, "patch").await.unwrap().status(), 503);
    assert_eq!(client.delete(&url).await.unwrap().status(), 503);

    let attempts: [u32; 7] = std::array::from_fn(|index| calls[index].load(Ordering::Relaxed));
    assert_eq!(attempts[0], RETRIES + 1, "safe GET may retry");
    assert_eq!(attempts[1], RETRIES + 1, "safe HEAD may retry");
    assert_eq!(attempts[2], RETRIES + 1, "safe OPTIONS may retry");
    assert_eq!(attempts[3], 1, "POST retried without explicit policy");
    assert_eq!(attempts[4], 1, "PUT retried without explicit policy");
    assert_eq!(attempts[5], 1, "PATCH retried without explicit policy");
    assert_eq!(attempts[6], 1, "DELETE retried without explicit policy");

    calls
        .iter()
        .for_each(|count| count.store(0, Ordering::Relaxed));
    let unsafe_retry_client = http::client()
        .retries(RETRIES)
        .backoff(Duration::from_millis(1))
        .retry_unsafe_methods(true);

    assert_eq!(
        unsafe_retry_client
            .post(&url, "post")
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(
        unsafe_retry_client.put(&url, "put").await.unwrap().status(),
        503
    );
    assert_eq!(
        unsafe_retry_client
            .patch(&url, "patch")
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(
        unsafe_retry_client.delete(&url).await.unwrap().status(),
        503
    );

    let opted_in_attempts: [u32; 4] =
        std::array::from_fn(|index| calls[index + 3].load(Ordering::Relaxed));
    assert_eq!(
        opted_in_attempts,
        [RETRIES + 1; 4],
        "explicit unsafe-method policy must permit retries"
    );

    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_method_transport_retry_requires_explicit_policy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/transport-retry", listener.local_addr().unwrap());
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(transport_failure_then_success(listener, completion_rx));

    let result = http::client()
        .retries(1)
        .backoff(Duration::from_millis(1))
        .post(&url, "body")
        .await;
    assert!(
        result.is_err(),
        "POST transport failure retried without opt-in"
    );
    completion_tx.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), upstream)
            .await
            .unwrap()
            .unwrap(),
        1
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/transport-retry", listener.local_addr().unwrap());
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(transport_failure_then_success(listener, completion_rx));
    let response = http::client()
        .retries(1)
        .backoff(Duration::from_millis(1))
        .retry_unsafe_methods(true)
        .post(&url, "body")
        .await;
    drop(completion_tx);
    let attempts = tokio::time::timeout(Duration::from_secs(2), upstream)
        .await
        .unwrap()
        .unwrap();
    let response = response.unwrap_or_else(|error| {
        panic!("opted-in POST transport retry failed after {attempts} accepted attempts: {error}")
    });

    assert_eq!(response.status(), 200);
    assert_eq!(response.body(), "ok");
    assert_eq!(attempts, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_retry_attempt_arithmetic_is_exact() {
    let calls = Arc::new(AtomicU32::new(0));
    let handler_calls = Arc::clone(&calls);
    let mut router = Router::new();
    router.get("/generated-retry", move |_req: &Request| {
        handler_calls.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    let server = crate::http::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let url = format!("http://{}/generated-retry", server.local_addr());
    let generator = crate::deterministic::DeterministicGenerator::stable();

    for index in 0..GENERATED_RETRY_CASES {
        let mut case = generator.case(index);
        let retries = case.bounded(RETRY_COUNT_BOUND) as u32;
        calls.store(0, Ordering::Relaxed);

        let response = http::client()
            .retries(retries)
            .backoff(Duration::from_millis(1))
            .get(&url)
            .await
            .unwrap();

        assert_eq!(response.status(), 503, "{case}: retries={retries}");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            retries + 1,
            "{case}: retries={retries}"
        );
    }

    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn transient_response_is_released_before_retry_backoff() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/retry-release", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move {
        let mut first = accept_request(&listener).await;
        first
            .write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 100\r\nConnection: keep-alive\r\n\r\n",
            )
            .await
            .unwrap();
        let mut byte = [0_u8; 1];
        let closed = tokio::time::timeout(Duration::from_millis(200), first.read(&mut byte))
            .await
            .expect("transient response stayed alive during retry backoff")
            .unwrap();
        assert_eq!(closed, 0, "transient response connection remained open");

        let mut second = accept_request(&listener).await;
        second
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });

    let response = http::client()
        .retries(1)
        .backoff(Duration::from_millis(500))
        .get(&url)
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    upstream.await.unwrap();
}

#[camber::test]
async fn client_retries_on_transient_error() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/retry", move |_req: &Request| {
        let n = c.fetch_add(1, Ordering::Relaxed);
        async move {
            match n < 2 {
                true => Response::empty(503),
                false => Response::text(200, "ok"),
            }
        }
    });
    let addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(3)
        .backoff(Duration::from_millis(10))
        .get(&format!("http://{addr}/retry"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "ok");
    assert_eq!(count.load(Ordering::Relaxed), 3);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_does_not_retry_on_4xx() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/bad", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async { Response::text(400, "bad request") }
    });
    let addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(3)
        .backoff(Duration::from_millis(10))
        .get(&format!("http://{addr}/bad"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    assert_eq!(count.load(Ordering::Relaxed), 1);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_exhausts_retries_and_returns_last_error() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/fail", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    let addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(2)
        .backoff(Duration::from_millis(10))
        .get(&format!("http://{addr}/fail"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 503);
    assert_eq!(count.load(Ordering::Relaxed), 3);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_free_functions_do_not_retry() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/once", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    let addr = common::spawn_server(backend);

    let resp = http::get(&format!("http://{addr}/once")).await.unwrap();

    assert_eq!(resp.status(), 503);
    assert_eq!(count.load(Ordering::Relaxed), 1);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_retries_on_timeout() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/slow", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async {
            std::thread::sleep(Duration::from_millis(200));
            Response::text(200, "slow")
        }
    });
    let addr = common::spawn_server(backend);

    let result = http::client()
        .retries(1)
        .backoff(Duration::from_millis(10))
        .read_timeout(Duration::from_millis(50))
        .get(&format!("http://{addr}/slow"))
        .await;

    match &result {
        Err(RuntimeError::Timeout) => {}
        Err(e) => panic!("expected Timeout, got error: {e}"),
        Ok(resp) => panic!("expected Timeout, got status {}", resp.status()),
    }

    assert_eq!(count.load(Ordering::Relaxed), 2);

    runtime::request_shutdown();
}
