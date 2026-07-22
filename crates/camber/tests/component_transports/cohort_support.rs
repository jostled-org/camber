use std::future::{Future, IntoFuture};
use std::time::Duration;

const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(PROTOCOL_TIMEOUT, future)
        .await
        .expect("transport protocol operation timed out")
}

pub(crate) async fn join_server(handle: camber::AsyncJoinHandle<Result<(), camber::RuntimeError>>) {
    bounded(handle.into_future()).await.unwrap().unwrap();
}
