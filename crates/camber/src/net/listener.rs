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
    let std_listener = std::os::unix::net::UnixListener::bind(&socket_path)?;
    let owned_path = OwnedSocketPath::new(socket_path)?;
    std_listener.set_nonblocking(true)?;
    let tokio_listener = tokio::net::UnixListener::from_std(std_listener)?;
    Ok(Listener {
        inner: ListenerInner::Unix(tokio_listener, owned_path),
    })
}

/// Bound TCP or Unix listener used by Camber server entrypoints.
pub struct Listener {
    pub(crate) inner: ListenerInner,
}

pub(crate) enum ListenerInner {
    Tcp(tokio::net::TcpListener),
    Unix(tokio::net::UnixListener, OwnedSocketPath),
}

pub(crate) struct OwnedSocketPath {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl OwnedSocketPath {
    fn new(path: PathBuf) -> Result<Self, RuntimeError> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn matches(&self, metadata: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        metadata.dev() == self.device && metadata.ino() == self.inode
    }
}

/// One transport a server accepted, whichever listener produced it.
///
/// The server supervisor owns both listener kinds through one accept loop, so
/// what it accepts has to be one type as well. TLS is a TCP concern: a Unix
/// peer is already confined to the host's filesystem permissions, and no entry
/// point has ever offered to wrap one.
pub(crate) enum AcceptedStream {
    Tcp(tokio::net::TcpStream),
    Unix(tokio::net::UnixStream),
}

impl AcceptedStream {
    /// Shut a transport down that this server will not serve.
    ///
    /// Every refusal — closed admission, an unavailable permit — goes through
    /// here, so a socket the server declines is closed rather than dropped raw.
    pub(crate) async fn close(self) {
        use tokio::io::AsyncWriteExt;
        match self {
            Self::Tcp(mut stream) => {
                let _ = stream.shutdown().await;
            }
            Self::Unix(mut stream) => {
                let _ = stream.shutdown().await;
            }
        }
    }
}

impl Listener {
    /// Adopt a listener the caller already bound on its own Tokio runtime.
    ///
    /// The owned serving entry points take a `tokio::net::TcpListener`, and the
    /// supervisor accepts through one type; this is where the two meet. A TCP
    /// listener owns no socket path, so it has nothing to clean up.
    pub(crate) fn from_tcp(listener: tokio::net::TcpListener) -> Self {
        Self {
            inner: ListenerInner::Tcp(listener),
        }
    }

    /// Accept one transport, whichever kind this listener binds.
    ///
    /// Cancel-safe: both inner accepts are, so a caller may race this in a
    /// `select!` and leave a pending connection queued on the listener.
    pub(crate) async fn accept(
        &self,
    ) -> Result<(AcceptedStream, Option<std::net::SocketAddr>), std::io::Error> {
        match &self.inner {
            ListenerInner::Tcp(listener) => {
                let (stream, addr) = listener.accept().await?;
                Ok((AcceptedStream::Tcp(stream), Some(addr)))
            }
            ListenerInner::Unix(listener, _) => {
                let (stream, _addr) = listener.accept().await?;
                Ok((AcceptedStream::Unix(stream), None))
            }
        }
    }

    /// Whether this listener binds a Unix socket rather than a TCP port.
    ///
    /// Asked by the builder that resolves TLS: a Unix peer is confined by the
    /// socket's filesystem permissions and no accept path wraps one, so the
    /// listener kind is what decides whether a certificate can apply at all.
    pub(crate) fn is_unix(&self) -> bool {
        matches!(self.inner, ListenerInner::Unix(_, _))
    }

    /// The bound TCP address, or `None` for a Unix socket.
    pub(crate) fn tcp_addr(&self) -> Option<std::net::SocketAddr> {
        match &self.inner {
            ListenerInner::Tcp(listener) => listener.local_addr().ok(),
            ListenerInner::Unix(_, _) => None,
        }
    }

    /// Returns the local address for TCP listeners, or the socket path for Unix listeners.
    pub fn local_addr(&self) -> Result<ListenerAddr, RuntimeError> {
        match &self.inner {
            ListenerInner::Tcp(l) => l
                .local_addr()
                .map(ListenerAddr::Tcp)
                .map_err(RuntimeError::from),
            ListenerInner::Unix(_, socket) => Ok(ListenerAddr::Unix(socket.path.clone())),
        }
    }

    /// Remove the Unix socket path for this listener.
    ///
    /// The fallible spelling, called by the owner that gives the listener up
    /// while it still has a caller to answer: a path replaced under a running
    /// service, or one on a directory the process cannot write, is a failure the
    /// serving terminal returns. `Drop` runs the same removal afterwards and can
    /// only log, so it is the account for a listener nothing ever served.
    pub fn cleanup(&self) -> Result<(), RuntimeError> {
        match &self.inner {
            ListenerInner::Tcp(_) => Ok(()),
            ListenerInner::Unix(_, socket) => cleanup_unix_socket(socket),
        }
    }

    /// The socket path this listener owns, for the log a failed removal leaves.
    fn socket_path(&self) -> Option<&std::path::Path> {
        match &self.inner {
            ListenerInner::Tcp(_) => None,
            ListenerInner::Unix(_, socket) => Some(&socket.path),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Err(err) = self.cleanup()
            && let Some(path) = self.socket_path()
        {
            tracing::warn!("failed to remove unix socket {}: {err}", path.display());
        }
    }
}

fn cleanup_unix_socket(socket: &OwnedSocketPath) -> Result<(), RuntimeError> {
    match std::fs::symlink_metadata(&socket.path) {
        Ok(metadata) if socket.matches(&metadata) => {
            std::fs::remove_file(&socket.path).map_err(RuntimeError::from)
        }
        Ok(_) => Err(RuntimeError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "unix socket path {} was replaced; refusing to delete it",
                socket.path.display()
            ),
        ))),
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
