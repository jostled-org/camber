/// Default maximum request body size (8 MB).
pub(crate) const DEFAULT_MAX_BODY: usize = 8 * 1024 * 1024;
/// Hard ceiling for request body size (256 MB).
pub(crate) const MAX_BODY_LIMIT: usize = 256 * 1024 * 1024;
/// Default channel buffer size for SSE and WebSocket connections.
pub(crate) const DEFAULT_CHANNEL_BUFFER: usize = 32;

/// Shared buffer-size configuration embedded by Router and HostRouter.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BufferConfig {
    pub(crate) max_request_body: usize,
    pub(crate) sse_buffer_size: usize,
    #[cfg(feature = "ws")]
    pub(crate) ws_buffer_size: usize,
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            max_request_body: DEFAULT_MAX_BODY,
            sse_buffer_size: DEFAULT_CHANNEL_BUFFER,
            #[cfg(feature = "ws")]
            ws_buffer_size: DEFAULT_CHANNEL_BUFFER,
        }
    }
}

impl BufferConfig {
    /// Set the maximum request body size in bytes (capped at 256 MB).
    pub(crate) fn with_max_request_body(mut self, bytes: usize) -> Self {
        self.max_request_body = bytes.min(MAX_BODY_LIMIT);
        self
    }

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
