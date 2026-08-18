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
    /// Every deadline this vocabulary declares.
    ///
    /// Named exhaustively so an operator-facing vocabulary can be published
    /// from the enum rather than transcribed beside it: a name a label may
    /// carry and a name this enum spells are the same list or they are two
    /// lists that drift.
    pub(super) const ALL: [Self; 15] = [
        Self::Header,
        Self::RequestBodyIdle,
        Self::RequestTotal,
        Self::TransferIdle,
        Self::TransferTotal,
        Self::ProxyConnect,
        Self::ProxyRequest,
        Self::ProxyUpstreamIdle,
        Self::ClientConnect,
        Self::ClientRequest,
        Self::ClientResponseIdle,
        Self::ResourceStartupHealth,
        Self::ResourcePeriodicHealth,
        Self::ResourceShutdown,
        Self::AggregateShutdown,
    ];

    /// The bounded name this deadline is reported and counted under.
    ///
    /// A closed vocabulary of static text. An operator's error message and, in
    /// time, an operator's metric label both read it, so it can never become a
    /// value derived from a request.
    pub(super) const fn label(self) -> &'static str {
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
    /// Every byte maximum this vocabulary declares, for the reason
    /// [`DeadlineBoundary::ALL`] exists.
    pub(super) const ALL: [Self; 7] = [
        Self::RequestBody,
        Self::TransferUpload,
        Self::TransferDownload,
        Self::ClientResponse,
        Self::ProxyBufferedResponse,
        Self::StaticFile,
        Self::ProfilingResponse,
    ];

    /// The bounded name this maximum is reported and counted under.
    ///
    /// A closed vocabulary of static text, for the reason
    /// [`DeadlineBoundary::label`] is one: an operator's error message and an
    /// operator's completion label both read it, so it can never become a value
    /// derived from a request.
    pub(super) const fn label(self) -> &'static str {
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

/// Which configured bound one service operation crossed, over both vocabularies.
///
/// A deadline and a byte maximum are different policies, but "which bound ended
/// this work" is one question, and the owners that answer it — a refusal, a
/// selected terminal, a completion record — each answer it once. Absence is a
/// name of its own rather than a missing value: a counter whose boundary label
/// is sometimes absent splits one time series into two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) enum CrossedBound {
    /// This operation crossed no configured bound.
    #[default]
    None,
    Deadline(DeadlineBoundary),
    Bytes(ByteBoundary),
}

impl CrossedBound {
    /// Every name a crossed-bound label may carry, over both vocabularies.
    ///
    /// Built from the two declared lists and the stated absence, so a published
    /// vocabulary and the bounds production can actually name are one list.
    pub(super) fn vocabulary() -> Box<[&'static str]> {
        std::iter::once(Self::None.label())
            .chain(DeadlineBoundary::ALL.map(|deadline| Self::Deadline(deadline).label()))
            .chain(ByteBoundary::ALL.map(|bytes| Self::Bytes(bytes).label()))
            .collect()
    }

    /// The bounded name this bound is reported and counted under.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Deadline(deadline) => deadline.label(),
            Self::Bytes(bytes) => bytes.label(),
        }
    }
}
