use crate::error::BenchError;
use std::net::SocketAddr;
use std::thread::JoinHandle;

pub struct ServerHandle {
    join_handle: JoinHandle<()>,
}

impl ServerHandle {
    pub(crate) fn new(join_handle: JoinHandle<()>) -> Self {
        Self { join_handle }
    }

    /// Wait for the server thread to finish.
    pub fn join(self) -> Result<(), BenchError> {
        self.join_handle.join().map_err(|payload| {
            let msg = match (
                payload.downcast_ref::<&str>(),
                payload.downcast_ref::<String>(),
            ) {
                (Some(s), _) => (*s).into(),
                (_, Some(s)) => s.as_str().into(),
                _ => Box::from("server thread panicked"),
            };
            BenchError::ServerStart(msg)
        })
    }
}

pub(crate) fn bind_and_spawn(
    setup: impl FnOnce(std::sync::mpsc::Sender<Result<SocketAddr, BenchError>>) + Send + 'static,
) -> Result<(SocketAddr, ServerHandle), BenchError> {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();

    let thread = std::thread::spawn(move || {
        let _ = camber::runtime::builder()
            .shutdown_timeout(std::time::Duration::from_secs(1))
            .run(|| setup(addr_tx));
    });

    let addr = addr_rx
        .recv()
        .map_err(|e| BenchError::ServerStart(e.to_string().into_boxed_str()))??;

    std::thread::sleep(std::time::Duration::from_millis(50));
    Ok((addr, ServerHandle::new(thread)))
}

pub(crate) fn bind_listener_and_send_addr(
    tx: &std::sync::mpsc::Sender<Result<SocketAddr, BenchError>>,
) -> Option<camber::net::Listener> {
    let listener = match camber::net::listen("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            let _ = tx.send(Err(BenchError::ServerStart(e.to_string().into_boxed_str())));
            return None;
        }
    };

    match listener.local_addr().ok().and_then(|a| a.tcp()) {
        Some(a) => {
            let _ = tx.send(Ok(a));
            Some(listener)
        }
        None => {
            let _ = tx.send(Err(BenchError::ServerStart(
                "failed to get local address".into(),
            )));
            None
        }
    }
}

pub(crate) fn require_upstream(
    bench: &str,
    upstream: Option<std::net::SocketAddr>,
) -> Result<std::net::SocketAddr, BenchError> {
    upstream.ok_or_else(|| {
        BenchError::ServerStart(format!("benchmark '{bench}' requires --upstream").into_boxed_str())
    })
}
