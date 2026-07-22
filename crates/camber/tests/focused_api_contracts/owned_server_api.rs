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

// 2.T6
#[test]
fn additive_lifecycle_api_compiles_through_exact_public_paths() {
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
