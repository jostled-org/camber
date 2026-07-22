use std::future::Future;
use std::sync::Arc;

use camber::RuntimeError;
use camber::http::{self, HostRouter, Router, ServerHandle};
use tokio::net::TcpListener;

fn expect_flat_server_future(future: impl Future<Output = Result<(), RuntimeError>>) {
    drop(future);
}

async fn published_embedded_server_calls(
    async_listener: TcpListener,
    async_tls_listener: TcpListener,
    async_hosts_listener: TcpListener,
    async_hosts_tls_listener: TcpListener,
    background_listener: TcpListener,
    background_tls_listener: TcpListener,
    background_hosts_listener: TcpListener,
    background_hosts_tls_listener: TcpListener,
    async_router: Router,
    async_tls_router: Router,
    async_host_router: HostRouter,
    async_host_tls_router: HostRouter,
    background_router: Router,
    background_tls_router: Router,
    background_host_router: HostRouter,
    background_host_tls_router: HostRouter,
    tls_config: Arc<rustls::ServerConfig>,
) {
    expect_flat_server_future(http::serve_async(async_listener, async_router));
    expect_flat_server_future(http::serve_async_tls(
        async_tls_listener,
        async_tls_router,
        Arc::clone(&tls_config),
    ));
    expect_flat_server_future(http::serve_async_hosts(
        async_hosts_listener,
        async_host_router,
    ));
    expect_flat_server_future(http::serve_async_hosts_tls(
        async_hosts_tls_listener,
        async_host_tls_router,
        Arc::clone(&tls_config),
    ));

    let background: ServerHandle = http::serve_background(background_listener, background_router);
    let background_tls: ServerHandle = http::serve_background_tls(
        background_tls_listener,
        background_tls_router,
        Arc::clone(&tls_config),
    );
    let background_hosts: ServerHandle =
        http::serve_background_hosts(background_hosts_listener, background_host_router);
    let background_hosts_tls: ServerHandle = http::serve_background_hosts_tls(
        background_hosts_tls_listener,
        background_host_tls_router,
        tls_config,
    );

    background.cancel();
    let result: Result<(), RuntimeError> = background.await;
    drop(result);
    drop(background_tls);
    drop(background_hosts);
    drop(background_hosts_tls);
}

#[test]
fn published_0_1_7_embedded_server_calls_compile_unchanged() {
    std::hint::black_box(published_embedded_server_calls);
}
