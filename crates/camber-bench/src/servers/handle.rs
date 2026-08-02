use crate::error::BenchError;
use std::net::SocketAddr;
use std::process::Child;
use std::thread::JoinHandle;

pub struct ServerHandle {
    owner: Option<ServerOwner>,
}

enum ServerOwner {
    Thread(JoinHandle<Result<(), BenchError>>),
    Process(Child),
}

impl ServerHandle {
    pub(crate) fn new(join_handle: JoinHandle<Result<(), BenchError>>) -> Self {
        Self {
            owner: Some(ServerOwner::Thread(join_handle)),
        }
    }

    pub(crate) fn from_child(child: Child) -> Self {
        Self {
            owner: Some(ServerOwner::Process(child)),
        }
    }

    /// Wait for the server thread to finish.
    pub fn join(mut self) -> Result<(), BenchError> {
        match self.owner.take() {
            Some(ServerOwner::Thread(join_handle)) => join_thread(join_handle),
            Some(ServerOwner::Process(mut child)) => wait_for_child(&mut child),
            None => Ok(()),
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(ServerOwner::Process(child)) = self.owner.as_mut() {
            terminate_child(child);
        }
    }
}

fn join_thread(join_handle: JoinHandle<Result<(), BenchError>>) -> Result<(), BenchError> {
    join_handle.join().map_err(|payload| {
        let message = match (
            payload.downcast_ref::<&str>(),
            payload.downcast_ref::<String>(),
        ) {
            (Some(message), _) => (*message).into(),
            (_, Some(message)) => message.as_str().into(),
            _ => Box::from("server thread panicked"),
        };
        BenchError::ServerStart(message)
    })?
}

fn wait_for_child(child: &mut Child) -> Result<(), BenchError> {
    let status = child.wait()?;
    match status.success() {
        true => Ok(()),
        false => Err(BenchError::ServerStart(
            format!("server process exited with {status}").into_boxed_str(),
        )),
    }
}

fn terminate_child(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) | Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) fn bind_and_spawn(
    setup: impl FnOnce(std::sync::mpsc::Sender<Result<SocketAddr, BenchError>>) + Send + 'static,
) -> Result<(SocketAddr, ServerHandle), BenchError> {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();

    let thread = std::thread::spawn(move || {
        camber::runtime::builder()
            .shutdown_timeout(std::time::Duration::from_secs(1))
            .run(|| setup(addr_tx))
            .map_err(|error| BenchError::ServerStart(error.to_string().into_boxed_str()))
    });

    let addr = addr_rx
        .recv()
        .map_err(|e| BenchError::ServerStart(e.to_string().into_boxed_str()))??;

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
