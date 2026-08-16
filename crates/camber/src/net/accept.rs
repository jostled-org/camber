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
/// This loop serves raw transports, which carry no connection limit: the one
/// listener semaphore belongs to the HTTP supervisor, which owns its permit for
/// the accepted transport's whole lifetime.
pub(crate) async fn accept_loop<L, F, Fut>(
    listener: &L,
    signals: &crate::runtime_state::LifecycleSignals,
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
    // every accepted connection.
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
        handlers.spawn(on_accept(connection));
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
