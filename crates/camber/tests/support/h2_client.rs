//! One HTTP/2 client, for the cases whose claim is about the h2 wire.
//!
//! Connect, handshake, spawn the driver, send the request, drain the body while
//! releasing flow-control capacity, drop the client, abort the driver, and read
//! the four outcomes that join can have. Two roots wrote that whole sequence out,
//! down to the panic strings, and every step of it is a step one copy could get
//! wrong on its own — a body drained without releasing capacity stalls the
//! sender, and a driver joined without reading its result discards a real fault
//! as a cancellation.
//!
//! The answer comes back as the same [`HttpResponse`] every other transport in
//! this suite hands over, so a root reading it needs no second answer type with
//! its own hand-copied accessors.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::http::{HttpResponse, bounded, remaining};

/// Send one HTTP/2 request over a connection of its own and read the answer.
///
/// The authority travels as `:authority`, which is where an HTTP/2 peer states
/// what an HTTP/1 peer states in `Host`. The connection is opened and given up
/// per request: what these cases turn on is what one exchange was answered with,
/// and a shared connection would make one row's framing a property of the row
/// before it.
///
/// `bound` is the whole exchange's, not each leg's. One deadline is taken here
/// and every leg is handed what is left of it, so the connect, the handshake,
/// the response head, and each body frame share the caller's budget instead of
/// each starting a fresh copy of it — which is the rule
/// [`super::http::remaining`] states for the whole harness, and a request that
/// spent a multiple of its bound was breaking it.
pub async fn h2_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
    bound: Duration,
) -> HttpResponse {
    let deadline = Instant::now() + bound;
    let tcp = bounded(
        tokio::net::TcpStream::connect(addr),
        remaining(deadline),
        "HTTP/2 connect",
    )
    .await
    .expect("the HTTP/2 peer could not connect");
    let (mut client, connection) = bounded(
        h2::client::handshake(tcp),
        remaining(deadline),
        "HTTP/2 handshake",
    )
    .await
    .expect("the HTTP/2 handshake did not complete");
    let driver = tokio::spawn(connection);

    let request = headers
        .iter()
        .fold(
            ::http::Request::builder()
                .method(method)
                .uri(format!("http://{host}{path}")),
            |builder, (name, value)| builder.header(*name, *value),
        )
        .body(())
        .expect("the HTTP/2 request head is representable");
    let (response, _) = client
        .send_request(request, true)
        .expect("the HTTP/2 stream could not be opened");
    let response = bounded(response, remaining(deadline), "HTTP/2 response head")
        .await
        .expect("no HTTP/2 response head");

    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                Box::from(name.as_str()),
                Box::from(String::from_utf8_lossy(value.as_bytes()).as_ref()),
            )
        })
        .collect();
    let body = drain_h2_body(
        response.into_body(),
        "HTTP/2 response body frame",
        remaining(deadline),
    )
    .await;

    drop(client);
    join_driver(driver).await;
    HttpResponse::from_parts(status, headers, body)
}

/// Read one HTTP/2 response body to end of stream.
///
/// Capacity is released per frame because the window is the sender's budget: a
/// reader that took the bytes and never released it would stall the peer partway
/// through a body, and the case waiting on that body would report a timeout for
/// a server that was doing exactly what it was told.
///
/// `bound` covers the whole body, not one frame of it. A per-frame budget gives
/// a peer dribbling frames a fresh full bound for each one, so a body that never
/// ends outlasts its caller's deadline by as many frames as the peer cares to
/// send.
pub async fn drain_h2_body(
    mut body: h2::RecvStream,
    operation: &str,
    bound: Duration,
) -> Box<[u8]> {
    let deadline = Instant::now() + bound;
    let mut bytes = Vec::new();
    while let Some(chunk) = bounded(body.data(), remaining(deadline), operation).await {
        let chunk = chunk.expect("an HTTP/2 body frame failed");
        body.flow_control()
            .release_capacity(chunk.len())
            .expect("the HTTP/2 reader could not release its flow-control capacity");
        bytes.extend_from_slice(&chunk);
    }
    bytes.into_boxed_slice()
}

/// End the connection driver and read the four outcomes that can have.
///
/// The response is already complete, so the driver is aborted rather than waited
/// on — the server's own keepalive policy decides when it would otherwise close.
/// Only the driver's own cancellation is an accepted end: a protocol failure or
/// a panic inside it is a fault, and a join that only checked for cancellation
/// would discard both.
async fn join_driver(driver: tokio::task::JoinHandle<Result<(), h2::Error>>) {
    driver.abort();
    match driver.await {
        Ok(Ok(())) => {}
        Err(error) if error.is_cancelled() => {}
        Ok(Err(error)) => panic!("HTTP/2 client driver failed: {error}"),
        Err(error) => panic!("HTTP/2 client driver join failed: {error}"),
    }
}
