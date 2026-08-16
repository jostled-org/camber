use std::fmt::Debug;
use std::future::{Future, IntoFuture};

use camber::RuntimeError;

fn assert_flat_future<F>()
where
    F: Future<Output = Result<(), RuntimeError>>,
{
}

fn assert_server_handle_into_future<T>()
where
    T: IntoFuture<Output = Result<(), RuntimeError>, IntoFuture = camber::http::ServerHandleFuture>,
{
}

fn assert_lifecycle_enum_traits<T>()
where
    T: Copy + Clone + Debug + Eq + PartialEq,
{
}

// 3.T2: the canonical builder and every fallible terminal
#[test]
fn additive_lifecycle_api_compiles_through_exact_public_paths() {
    // The canonical owned server path, named exactly as documentation presents
    // it. Every terminal is fallible, and the two owner-producing terminals
    // hand back the same owners the free functions do.
    let _: fn(camber::http::Router) -> camber::http::ServerBuilder = camber::http::server;
    let _: fn(camber::http::HostRouter) -> camber::http::ServerBuilder = camber::http::server_hosts;
    let _: fn(
        camber::http::ServerBuilder,
        camber::http::ServerPolicy,
    ) -> camber::http::ServerBuilder = camber::http::ServerBuilder::policy;
    let _: fn(camber::http::ServerBuilder, &str) -> Result<(), RuntimeError> =
        camber::http::ServerBuilder::serve;
    let _: fn(camber::http::ServerBuilder, camber::net::Listener) -> Result<(), RuntimeError> =
        camber::http::ServerBuilder::serve_listener;
    let _: fn(
        camber::http::ServerBuilder,
        tokio::net::TcpListener,
    ) -> Result<camber::http::ServerHandleFuture, RuntimeError> =
        camber::http::ServerBuilder::serve_async;
    let _: fn(
        camber::http::ServerBuilder,
        tokio::net::TcpListener,
    ) -> Result<camber::http::ServerHandle, RuntimeError> =
        camber::http::ServerBuilder::serve_background;

    let _: fn(&camber::http::ServerHandle) = camber::http::ServerHandle::shutdown;
    let _: fn(camber::http::ServerHandle) -> camber::http::ServerHandleFuture =
        camber::http::ServerHandle::join;
    let _: fn(camber::http::ServerHandle) -> camber::http::ServerHandleFuture =
        camber::http::ServerHandle::shutdown_and_join;
    let _: fn(&camber::http::ServerHandleFuture) = camber::http::ServerHandleFuture::shutdown;
    let _: fn(&camber::http::ServerHandleFuture) = camber::http::ServerHandleFuture::cancel;

    assert_flat_future::<camber::http::ServerHandleFuture>();
    assert_server_handle_into_future::<camber::http::ServerHandle>();

    assert_lifecycle_enum_traits::<camber::http::mock::LifecycleCheckpoint>();
    assert_lifecycle_enum_traits::<camber::http::mock::LifecycleFault>();
    assert_lifecycle_enum_traits::<camber::http::mock::SupervisorJoinProbe>();
}
