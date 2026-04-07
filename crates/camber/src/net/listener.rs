use crate::RuntimeError;
use std::path::PathBuf;

/// Bind a listener to the given address.
///
/// Supports two address formats:
/// - `"unix:/path/to/sock"` — binds a Unix domain socket
/// - `"host:port"` or `":port"` — binds a TCP listener
pub fn listen(addr: &str) -> Result<Listener, RuntimeError> {
    match addr.strip_prefix("unix:") {
        Some(path) => listen_unix(path),
        None => listen_tcp(addr),
    }
}

fn listen_tcp(addr: &str) -> Result<Listener, RuntimeError> {
    let std_listener = std::net::TcpListener::bind(addr)?;
    std_listener.set_nonblocking(true)?;
    let tokio_listener = tokio::net::TcpListener::from_std(std_listener)?;
    Ok(Listener {
        inner: ListenerInner::Tcp(tokio_listener),
    })
}

fn listen_unix(path: &str) -> Result<Listener, RuntimeError> {
    let socket_path = PathBuf::from(path);

    // Remove stale socket file if it exists
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    let std_listener = std::os::unix::net::UnixListener::bind(&socket_path)?;
    std_listener.set_nonblocking(true)?;
    let tokio_listener = tokio::net::UnixListener::from_std(std_listener)?;
    Ok(Listener {
        inner: ListenerInner::Unix(tokio_listener, socket_path),
    })
}

/// Bound TCP or Unix listener used by Camber server entrypoints.
pub struct Listener {
    pub(crate) inner: ListenerInner,
}

pub(crate) enum ListenerInner {
    Tcp(tokio::net::TcpListener),
    Unix(tokio::net::UnixListener, PathBuf),
}

impl Listener {
    /// Returns the local address for TCP listeners, or the socket path for Unix listeners.
    pub fn local_addr(&self) -> Result<ListenerAddr, RuntimeError> {
        match &self.inner {
            ListenerInner::Tcp(l) => l
                .local_addr()
                .map(ListenerAddr::Tcp)
                .map_err(RuntimeError::from),
            ListenerInner::Unix(_, path) => Ok(ListenerAddr::Unix(path.clone())),
        }
    }

    /// Remove the Unix socket path for this listener.
    pub fn cleanup(&self) -> Result<(), RuntimeError> {
        match &self.inner {
            ListenerInner::Tcp(_) => Ok(()),
            ListenerInner::Unix(_, path) => cleanup_unix_socket(path),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let ListenerInner::Unix(_, path) = &self.inner
            && let Err(err) = cleanup_unix_socket(path)
        {
            tracing::warn!("failed to remove unix socket {}: {err}", path.display());
        }
    }
}

fn cleanup_unix_socket(path: &std::path::Path) -> Result<(), RuntimeError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Address of a bound listener.
pub enum ListenerAddr {
    /// TCP socket address.
    Tcp(std::net::SocketAddr),
    /// Unix domain socket path.
    Unix(PathBuf),
}

impl ListenerAddr {
    /// Returns the TCP socket address, or `None` for Unix sockets.
    pub fn tcp(self) -> Option<std::net::SocketAddr> {
        match self {
            ListenerAddr::Tcp(addr) => Some(addr),
            ListenerAddr::Unix(_) => None,
        }
    }
}

impl std::fmt::Display for ListenerAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListenerAddr::Tcp(addr) => write!(f, "{addr}"),
            ListenerAddr::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}
