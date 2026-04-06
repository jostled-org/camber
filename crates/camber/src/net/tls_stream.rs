use crate::RuntimeError;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Async TLS stream wrapping either a client or server `tokio_rustls` stream.
///
/// Provides the same read/write API as `TcpStream`, plus `peer_certificates()`
/// for TLS inspection (e.g. cert probing).
pub struct TlsStream {
    inner: TlsStreamInner,
}

enum TlsStreamInner {
    Client(tokio_rustls::client::TlsStream<tokio::net::TcpStream>),
    Server(tokio_rustls::server::TlsStream<tokio::net::TcpStream>),
}

macro_rules! dispatch {
    ($self:expr, |$s:ident| $body:expr) => {
        match &mut $self.inner {
            TlsStreamInner::Client($s) => $body,
            TlsStreamInner::Server($s) => $body,
        }
    };
    (ref $self:expr, |$s:ident| $body:expr) => {
        match &$self.inner {
            TlsStreamInner::Client($s) => $body,
            TlsStreamInner::Server($s) => $body,
        }
    };
}

impl std::fmt::Debug for TlsStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsStream")
            .field("peer_addr", &self.peer_addr().ok())
            .finish()
    }
}

impl TlsStream {
    pub(crate) fn from_client(
        stream: tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    ) -> Self {
        Self {
            inner: TlsStreamInner::Client(stream),
        }
    }

    pub(crate) fn from_server(
        stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    ) -> Self {
        Self {
            inner: TlsStreamInner::Server(stream),
        }
    }

    /// Read data into the buffer, returning the number of bytes read.
    pub async fn read(&mut self, read_buf: &mut [u8]) -> Result<usize, RuntimeError> {
        let bytes_read = dispatch!(self, |s| s.read(read_buf).await?);
        Ok(bytes_read)
    }

    /// Write all bytes from the buffer.
    pub async fn write_all(&mut self, write_buf: &[u8]) -> Result<(), RuntimeError> {
        dispatch!(self, |s| s.write_all(write_buf).await?);
        Ok(())
    }

    /// Shut down the write half of the stream.
    pub async fn shutdown(&mut self) -> Result<(), RuntimeError> {
        dispatch!(self, |s| s.shutdown().await?);
        Ok(())
    }

    /// Returns the remote address this stream is connected to.
    pub fn peer_addr(&self) -> Result<SocketAddr, RuntimeError> {
        let addr = dispatch!(ref self, |s| s.get_ref().0).peer_addr()?;
        Ok(addr)
    }

    /// Returns the local address this stream is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, RuntimeError> {
        let addr = dispatch!(ref self, |s| s.get_ref().0).local_addr()?;
        Ok(addr)
    }

    /// Returns the peer's TLS certificates, if available.
    pub fn peer_certificates(&self) -> Option<&[rustls::pki_types::CertificateDer<'static>]> {
        dispatch!(ref self, |s| s.get_ref().1.peer_certificates())
    }
}

impl tokio::io::AsyncRead for TlsStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut self.inner {
            TlsStreamInner::Client(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            TlsStreamInner::Server(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for TlsStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut self.inner {
            TlsStreamInner::Client(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            TlsStreamInner::Server(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut self.inner {
            TlsStreamInner::Client(s) => std::pin::Pin::new(s).poll_flush(cx),
            TlsStreamInner::Server(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut self.inner {
            TlsStreamInner::Client(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            TlsStreamInner::Server(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}
