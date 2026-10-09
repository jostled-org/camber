use std::future::Future;

/// Executors that must make progress without a spare callback worker.
#[derive(Clone, Copy, Debug)]
pub enum WsExecutor {
    CurrentThread,
    OneWorker,
}

impl WsExecutor {
    pub const ALL: [Self; 2] = [Self::CurrentThread, Self::OneWorker];

    /// Drive a fresh case outside any enclosing Tokio runtime.
    pub fn run<F, Fut>(self, body: F)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        match self {
            Self::CurrentThread => super::drain::block_on_detached(body()),
            Self::OneWorker => tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap_or_else(|error| panic!("{self:?}: cannot build runtime: {error}"))
                .block_on(body()),
        }
    }
}

/// Repeat one async case with fresh state on each executor.
pub fn on_ws_executors<F, Fut>(body: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    for executor in WsExecutor::ALL {
        executor.run(&body);
    }
}
