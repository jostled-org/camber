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
/// Returns `Ok(())` when either lifecycle signal fires. Returns `Err` on fatal
/// accept errors. Transient errors (fd exhaustion) trigger a 100ms backoff.
///
/// The pair, not the shutdown latch alone. The user closure's return closes
/// root-scope admission and fires `ScopeClosing` with the shutdown latch left
/// unset, so a loop watching shutdown alone would still be parked at the
/// escalation boundary and turn a clean exit into a scope drain timeout.
///
/// When `conn_limit` is `Some`, the semaphore bounds the number of concurrent
/// connections. The accept loop waits for a permit before spawning a task;
/// the permit is released when the connection task completes.
pub(crate) async fn accept_loop<L, F, Fut>(
    listener: &L,
    signals: &crate::runtime_state::LifecycleSignals,
    conn_limit: Option<&Arc<tokio::sync::Semaphore>>,
    on_accept: F,
) -> Result<(), RuntimeError>
where
    L: Acceptor,
    F: Fn(L::Accepted) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    // Registered once for the listener, not once per connection. Both latches
    // are sticky, so a wait that has not resolved is still the same wait next
    // time round; constructing it inside the loop would register and deregister
    // a `Notify` waiter — an internal mutex and an intrusive-list edit — on
    // every accepted connection, and again on every permit wait.
    let stop = signals.wait();
    tokio::pin!(stop);
    let mut handlers = tokio::task::JoinSet::new();
    loop {
        // `biased` gives the stop signals priority over a ready connection, and
        // states the stop condition once. The accept future is dropped unpolled
        // when they win, so the connection stays queued on the listener.
        let event = tokio::select! {
            biased;
            () = &mut stop => AcceptLoopEvent::Stop,
            result = handlers.join_next(), if !handlers.is_empty() => {
                AcceptLoopEvent::HandlerFinished(result)
            }
            result = listener.accept() => AcceptLoopEvent::Accepted(result),
        };
        let accepted = match event {
            AcceptLoopEvent::Stop => {
                stop_handlers(&mut handlers).await;
                return Ok(());
            }
            AcceptLoopEvent::HandlerFinished(Some(Ok(())) | None) => continue,
            AcceptLoopEvent::HandlerFinished(Some(Err(error))) => {
                tracing::warn!(%error, "transport handler task failed");
                continue;
            }
            AcceptLoopEvent::Accepted(result) => result,
        };
        let connection = match accepted {
            Ok(connection) => connection,
            Err(error) if crate::error::is_transient_accept_error(&error) => {
                tracing::warn!("accept: fd limit reached, backing off");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
            Err(error) => {
                stop_handlers(&mut handlers).await;
                return Err(error.into());
            }
        };
        match spawn_with_limit(
            conn_limit,
            stop.as_mut(),
            &mut handlers,
            on_accept(connection),
        )
        .await
        {
            true => {
                stop_handlers(&mut handlers).await;
                return Ok(());
            }
            false => {}
        }
    }
}

enum AcceptLoopEvent<T> {
    Stop,
    Accepted(Result<T, std::io::Error>),
    HandlerFinished(Option<Result<(), tokio::task::JoinError>>),
}

async fn stop_handlers(handlers: &mut tokio::task::JoinSet<()>) {
    handlers.abort_all();
    while handlers.join_next().await.is_some() {}
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
    // Registered once for the listener, not once per connection — the same
    // reason `accept_loop` hoists its wait. This loop would otherwise pay the
    // registration twice per connection: once on the accept, once on the permit.
    let stop = shutdown.wait();
    tokio::pin!(stop);
    loop {
        let accepted = tokio::select! {
            biased;
            () = &mut stop => return Ok(()),
            result = listener.accept() => result,
        };
        let connection = match accepted {
            Ok(connection) => connection,
            Err(error) if crate::error::is_transient_accept_error(&error) => {
                tracing::warn!("accept: fd limit reached, backing off");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let permit = match conn_limit {
            Some(_) => tokio::select! {
                biased;
                () = &mut stop => return Ok(()),
                permit = acquire_connection_permit(conn_limit, script.map(Arc::as_ref)) => permit.ok(),
            },
            None => None,
        };
        if conn_limit.is_none() || permit.is_some() {
            tokio::spawn(on_accept(connection, permit));
        }
    }
}

pub(crate) async fn acquire_connection_permit(
    conn_limit: Option<&Arc<tokio::sync::Semaphore>>,
    script: Option<&crate::http::mock::LifecycleScript>,
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
///
/// `stop` is the loop's own hoisted wait, borrowed rather than re-derived: the
/// caller already holds one registration for the whole listener, and deriving a
/// second one here would restore the per-connection cost the hoist removed.
/// Generic over the future so the caller decides which signals it races.
async fn spawn_with_limit<Stop, Fut>(
    conn_limit: Option<&Arc<tokio::sync::Semaphore>>,
    stop: Pin<&mut Stop>,
    handlers: &mut tokio::task::JoinSet<()>,
    fut: Fut,
) -> bool
where
    Stop: Future<Output = ()>,
    Fut: Future<Output = ()> + Send + 'static,
{
    let permit = match conn_limit {
        None => {
            handlers.spawn(fut);
            return false;
        }
        Some(sem) => tokio::select! {
            biased;
            () = stop => {
                return true;
            }
            permit = Arc::clone(sem).acquire_owned() => permit,
        },
    };
    if let Ok(permit) = permit {
        handlers.spawn(async move {
            fut.await;
            drop(permit);
        });
    }
    false
}

/// Report one user handler's outcome against the transport that dispatched to
/// it.
///
/// A benign IO error is the peer hanging up mid-exchange: the ordinary end of an
/// exchange, not a fault to report. Every transport that dispatches to a user
/// handler answers it here, so the rule has one definition and a transport
/// cannot quietly adopt a different one. It lives beside the loops for the same
/// reason — this module is the transport-neutral half, so no reader has to open
/// one transport's file to find another's error policy. Named for the handler
/// rather than the connection, because UDP has no connection to name.
///
/// The transport is a structured FIELD, not part of the message. Interpolating
/// it would give the three transports three distinct message strings for one
/// condition, which is the thing an operator filters on — and would make
/// `tracing` format a message per event that a field carries for free.
pub(super) fn report_handler_error(transport: &'static str, result: Result<(), RuntimeError>) {
    match result {
        Ok(()) => {}
        Err(error) if crate::error::is_benign_io_error(&error) => {}
        Err(error) => tracing::warn!(transport, %error, "transport handler failed"),
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
