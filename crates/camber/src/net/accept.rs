use crate::RuntimeError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

/// Listener that can accept connections.
///
/// Abstracts over TCP and Unix listeners so the accept loop
/// can be written once for both transport types.
pub(crate) trait Acceptor {
    /// The value produced by accepting a connection.
    type Accepted;

    /// Accept a single connection. Must be cancel-safe.
    fn accept(&self) -> impl Future<Output = Result<Self::Accepted, std::io::Error>> + Send + '_;
}

impl Acceptor for tokio::net::TcpListener {
    type Accepted = (tokio::net::TcpStream, std::net::SocketAddr);

    fn accept(&self) -> impl Future<Output = Result<Self::Accepted, std::io::Error>> + Send + '_ {
        tokio::net::TcpListener::accept(self)
    }
}

impl Acceptor for tokio::net::UnixListener {
    type Accepted = tokio::net::UnixStream;

    async fn accept(&self) -> Result<Self::Accepted, std::io::Error> {
        let (stream, _addr) = tokio::net::UnixListener::accept(self).await?;
        Ok(stream)
    }
}

/// Run an accept loop, dispatching each connection to `on_accept`.
///
/// Returns `Ok(())` when shutdown is requested. Returns `Err` on fatal
/// accept errors. Transient errors (fd exhaustion) trigger a 100ms backoff.
///
/// When `conn_limit` is `Some`, the semaphore bounds the number of concurrent
/// connections. The accept loop waits for a permit before spawning a task;
/// the permit is released when the connection task completes.
pub(crate) async fn accept_loop<L, F, Fut>(
    listener: &L,
    shutdown: &crate::runtime_state::ShutdownSignal,
    conn_limit: Option<&Arc<tokio::sync::Semaphore>>,
    on_accept: F,
) -> Result<(), RuntimeError>
where
    L: Acceptor,
    F: Fn(L::Accepted) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        tokio::select! {
            biased;
            () = shutdown.wait() => {
                return Ok(());
            }
            result = listener.accept() => {
                match result {
                    Ok(accepted) => {
                        match spawn_with_limit(
                            conn_limit,
                            shutdown,
                            on_accept(accepted),
                        ).await {
                            true => return Ok(()),
                            false => {}
                        }
                    }
                    Err(e) if crate::error::is_transient_accept_error(&e) => {
                        tracing::warn!("accept: fd limit reached, backing off");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
}

/// Run the synchronous HTTP accept loop while transferring an acquired permit
/// into the connection future. The lifecycle script can observe only the real
/// pending semaphore acquisition.
pub(crate) async fn accept_loop_with_permit<L, F, Fut>(
    listener: &L,
    shutdown: &crate::runtime_state::ShutdownSignal,
    conn_limit: Option<&Arc<tokio::sync::Semaphore>>,
    script: Option<&Arc<crate::http::mock::LifecycleScript>>,
    on_accept: F,
) -> Result<(), RuntimeError>
where
    L: Acceptor,
    F: Fn(L::Accepted, Option<tokio::sync::OwnedSemaphorePermit>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        tokio::select! {
            biased;
            () = shutdown.wait() => {
                return Ok(());
            }
            result = listener.accept() => {
                match result {
                    Ok(accepted) => {
                        let permit = match conn_limit {
                            Some(_) => tokio::select! {
                                biased;
                                () = shutdown.wait() => return Ok(()),
                                permit = acquire_connection_permit(conn_limit, script) => permit.ok(),
                            },
                            None => None,
                        };
                        if conn_limit.is_none() || permit.is_some() {
                            tokio::spawn(on_accept(accepted, permit));
                        }
                    }
                    Err(error) if crate::error::is_transient_accept_error(&error) => {
                        tracing::warn!("accept: fd limit reached, backing off");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
}

pub(crate) async fn acquire_connection_permit(
    conn_limit: Option<&Arc<tokio::sync::Semaphore>>,
    script: Option<&Arc<crate::http::mock::LifecycleScript>>,
) -> Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError> {
    let semaphore = match conn_limit {
        Some(semaphore) => Arc::clone(semaphore),
        None => return std::future::pending().await,
    };
    let future = semaphore.acquire_owned();
    tokio::pin!(future);
    let immediate =
        std::future::poll_fn(
            |context| match Future::poll(Pin::new(&mut future), context) {
                Poll::Ready(result) => Poll::Ready(Some(result)),
                Poll::Pending => Poll::Ready(None),
            },
        )
        .await;
    match (immediate, script) {
        (Some(result), _) => result,
        (None, Some(script)) => {
            script
                .pause(crate::http::mock::LifecycleCheckpoint::ConnectionPermitWaitPending)
                .await;
            future.await
        }
        (None, None) => future.await,
    }
}

/// Spawn a connection task, optionally gated by a semaphore permit.
///
/// When `conn_limit` is `None`, spawns immediately. When `Some`, acquires a
/// permit first. The permit is held for the lifetime of the spawned task,
/// so it is released when the connection closes. Closed semaphores (runtime
/// shutdown) are treated as a no-op — the connection is dropped silently.
async fn spawn_with_limit<Fut>(
    conn_limit: Option<&Arc<tokio::sync::Semaphore>>,
    shutdown: &crate::runtime_state::ShutdownSignal,
    fut: Fut,
) -> bool
where
    Fut: Future<Output = ()> + Send + 'static,
{
    let permit = match conn_limit {
        None => {
            tokio::spawn(fut);
            return false;
        }
        Some(sem) => tokio::select! {
            biased;
            () = shutdown.wait() => {
                return true;
            }
            permit = Arc::clone(sem).acquire_owned() => permit,
        },
    };
    if let Ok(permit) = permit {
        tokio::spawn(async move {
            fut.await;
            drop(permit);
        });
    }
    false
}

/// Perform a TLS handshake with a 10-second timeout.
///
/// Returns `Some(tls_stream)` on success, `None` on timeout, benign IO errors,
/// or handshake failures. Non-benign failures are logged as warnings.
pub(crate) async fn tls_handshake(
    stream: tokio::net::TcpStream,
    acceptor: &tokio_rustls::TlsAcceptor,
) -> Option<tokio_rustls::server::TlsStream<tokio::net::TcpStream>> {
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(stream)).await;
    match result {
        Ok(Ok(s)) => Some(s),
        Ok(Err(e)) if crate::error::is_benign_io(&e) => None,
        Ok(Err(e)) => {
            tracing::warn!("TLS handshake error: {e}");
            None
        }
        Err(_) => None,
    }
}
