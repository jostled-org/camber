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

impl DeadlineBoundary {
    /// The bounded name this deadline is reported and counted under.
    ///
    /// A closed vocabulary of static text. An operator's error message and, in
    /// time, an operator's metric label both read it, so it can never become a
    /// value derived from a request.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Header => "header",
            Self::RequestBodyIdle => "request_body_idle",
            Self::RequestTotal => "request_total",
            Self::TransferIdle => "transfer_idle",
            Self::TransferTotal => "transfer_total",
            Self::ProxyConnect => "proxy_connect",
            Self::ProxyRequest => "proxy_request",
            Self::ProxyUpstreamIdle => "proxy_upstream_idle",
            Self::ClientConnect => "client_connect",
            Self::ClientRequest => "client_request",
            Self::ClientResponseIdle => "client_response_idle",
            Self::ResourceStartupHealth => "resource_startup_health",
            Self::ResourcePeriodicHealth => "resource_periodic_health",
            Self::ResourceShutdown => "resource_shutdown",
            Self::AggregateShutdown => "aggregate_shutdown",
        }
    }
}

impl std::fmt::Display for DeadlineBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
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

impl ByteBoundary {
    /// The bounded name this maximum is reported and counted under.
    ///
    /// A closed vocabulary of static text, for the reason
    /// [`DeadlineBoundary::label`] is one: an operator's error message reads
    /// it, so it can never become a value derived from a request. Private
    /// until an owner outside this module reports one, which is the same rule
    /// that would widen it.
    const fn label(self) -> &'static str {
        match self {
            Self::RequestBody => "request_body",
            Self::TransferUpload => "transfer_upload",
            Self::TransferDownload => "transfer_download",
            Self::ClientResponse => "client_response",
            Self::ProxyBufferedResponse => "proxy_buffered_response",
            Self::StaticFile => "static_file",
            Self::ProfilingResponse => "profiling_response",
        }
    }
}

impl std::fmt::Display for ByteBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}
