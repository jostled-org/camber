/// Default channel buffer size for SSE and WebSocket connections.
pub(crate) const DEFAULT_CHANNEL_BUFFER: usize = 32;

/// Shared channel-depth configuration embedded by Router and HostRouter.
///
/// Request-body limits are not here: those belong to the route a request
/// matched, and are resolved by routing rather than carried on the connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BufferConfig {
    pub(crate) sse_buffer_size: usize,
    #[cfg(feature = "ws")]
    pub(crate) ws_buffer_size: usize,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            sse_buffer_size: DEFAULT_CHANNEL_BUFFER,
            #[cfg(feature = "ws")]
            ws_buffer_size: DEFAULT_CHANNEL_BUFFER,
        }
    }
}

impl BufferConfig {
    /// Set the channel buffer size for SSE connections (minimum 1).
    pub(crate) fn with_sse_buffer_size(mut self, size: usize) -> Self {
        self.sse_buffer_size = size.max(1);
        self
    }

    /// Set the channel buffer size for WebSocket connections (minimum 1).
    #[cfg(feature = "ws")]
    pub(crate) fn with_ws_buffer_size(mut self, size: usize) -> Self {
        self.ws_buffer_size = size.max(1);
        self
    }
}
