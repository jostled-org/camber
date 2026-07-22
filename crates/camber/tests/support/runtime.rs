use std::net::SocketAddr;
use std::time::Duration;

use camber::http::Router;
use camber::{RuntimeError, http, runtime, spawn};

use super::http::wait_for_http_response;

pub fn test_runtime() -> runtime::RuntimeBuilder {
    runtime::builder()
        .keepalive_timeout(Duration::from_millis(100))
        .shutdown_timeout(Duration::from_secs(1))
}

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

pub fn spawn_server(router: Router) -> SocketAddr {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let local_addr = listener.local_addr().unwrap().tcp().unwrap();
    spawn(move || -> Result<(), RuntimeError> { http::serve_listener(listener, router) });
    wait_for_http_response(local_addr, Duration::from_secs(5)).unwrap();
    local_addr
}
