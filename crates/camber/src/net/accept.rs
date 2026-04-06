use crate::RuntimeError;
use std::future::Future;
use std::sync::Arc;

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
/// Returns `Ok(())` when `shutdown_notify` fires. Returns `Err` on fatal
/// accept errors. Transient errors (fd exhaustion) trigger a 100ms backoff.
///
/// When `conn_limit` is `Some`, the semaphore bounds the number of concurrent
/// connections. The accept loop waits for a permit before spawning a task;
/// the permit is released when the connection task completes.
pub(crate) async fn accept_loop<L, F, Fut>(
    listener: &L,
    shutdown_notify: &tokio::sync::Notify,
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
            result = listener.accept() => {
                match result {
                    Ok(accepted) => {
                        spawn_with_limit(conn_limit, on_accept(accepted)).await;
                    }
                    Err(e) if crate::error::is_transient_accept_error(&e) => {
                        tracing::warn!("accept: fd limit reached, backing off");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            () = shutdown_notify.notified() => {
                return Ok(());
            }
        }
    }
}

/// Spawn a connection task, optionally gated by a semaphore permit.
///
/// When `conn_limit` is `None`, spawns immediately. When `Some`, acquires a
/// permit first. The permit is held for the lifetime of the spawned task,
/// so it is released when the connection closes. Closed semaphores (runtime
/// shutdown) are treated as a no-op — the connection is dropped silently.
async fn spawn_with_limit<Fut>(conn_limit: Option<&Arc<tokio::sync::Semaphore>>, fut: Fut)
where
    Fut: Future<Output = ()> + Send + 'static,
{
    let permit = match conn_limit {
        None => {
            tokio::spawn(fut);
            return;
        }
        Some(sem) => Arc::clone(sem).acquire_owned().await,
    };
    if let Ok(permit) = permit {
        tokio::spawn(async move {
            fut.await;
            drop(permit);
        });
    }
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
