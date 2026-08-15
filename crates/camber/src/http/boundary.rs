//! The closed vocabulary of bounds a service operation can cross.
//!
//! Every deadline and byte maximum Camber enforces names itself with one of
//! these values, so an operator reading a failure learns which configured bound
//! ended the work rather than that "something timed out". Both enums are closed
//! and exhaustively matchable: a new bound is a deliberate API change, not a
//! silent addition a caller's `match` quietly ignores.

/// The configured deadline a service operation crossed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeadlineBoundary {
    /// The pre-head wait for a complete request head.
    Header,
    /// The quiet interval allowed between request body data frames.
    RequestBodyIdle,
    /// The lifetime from admitted request head to committed response head.
    RequestTotal,
    /// The quiet interval allowed between frames of one streaming transfer.
    TransferIdle,
    /// The lifetime of one streaming transfer.
    TransferTotal,
    /// Establishing the upstream transport for a proxied request.
    ProxyConnect,
    /// The proxied request from construction through a usable upstream head.
    ProxyRequest,
    /// The quiet interval allowed between upstream response body frames.
    ProxyUpstreamIdle,
    /// Establishing the transport for an outbound client request.
    ClientConnect,
    /// One complete outbound client attempt.
    ClientRequest,
    /// The quiet interval allowed between outbound response body frames.
    ClientResponseIdle,
    /// A resource's initial readiness health callback.
    ResourceStartupHealth,
    /// A resource's periodic health callback.
    ResourcePeriodicHealth,
    /// A resource's shutdown callback.
    ResourceShutdown,
    /// The one deadline every graceful shutdown participant shares.
    AggregateShutdown,
}

/// The configured byte maximum a service operation crossed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ByteBoundary {
    /// The route-aware maximum an admitted request body was read under.
    RequestBody,
    /// The maximum of one streaming upload.
    TransferUpload,
    /// The maximum of one streaming download.
    TransferDownload,
    /// The buffered maximum of an outbound client response.
    ClientResponse,
    /// The buffered maximum of a proxied upstream response.
    ProxyBufferedResponse,
    /// The maximum a static file is read and retained under.
    StaticFile,
    /// The maximum a rendered profiling response is retained under.
    ProfilingResponse,
}
