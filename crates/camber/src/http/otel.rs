use super::middleware::{MiddlewareFn, Next};
use super::{Request, Response};
use arrayvec::ArrayString;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use tracing::Instrument;

/// W3C Trace Context stored in task-local during request handling.
#[derive(Clone, Copy)]
pub(crate) struct TraceContext {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    flags: u8,
}

tokio::task_local! {
    static CURRENT_CONTEXT: Option<TraceContext>;
}

/// Returns the current request's W3C `traceparent` header value.
///
/// Call from a handler running inside `otel::tracing()` middleware to read
/// the propagated trace context. Returns `None` outside middleware scope.
pub fn current_traceparent() -> Option<Box<str>> {
    CURRENT_CONTEXT
        .try_with(|ctx| ctx.map(TraceContext::as_traceparent))
        .ok()
        .flatten()
}

/// Read raw trace context for outbound header injection.
pub(crate) fn current_context() -> Option<TraceContext> {
    CURRENT_CONTEXT.try_with(|ctx| *ctx).ok().flatten()
}

impl TraceContext {
    fn as_traceparent(self) -> Box<str> {
        let s = self.format_traceparent();
        Box::from(s.as_str())
    }

    /// Format as W3C traceparent into a stack-allocated buffer.
    /// Exact format: `00-{32hex}-{16hex}-{2hex}` = 55 chars.
    pub(crate) fn format_traceparent(self) -> ArrayString<55> {
        // Buffer is exactly sized — writes cannot overflow — but we propagate
        // errors via the inner helper to satisfy the fallible API contract.
        self.format_traceparent_inner()
            .unwrap_or_else(|_| ArrayString::new())
    }

    fn format_traceparent_inner(self) -> Result<ArrayString<55>, fmt::Error> {
        const HEX: [u8; 16] = *b"0123456789abcdef";
        let mut buf = ArrayString::new();
        fmt::Write::write_str(&mut buf, "00-")?;
        for b in &self.trace_id {
            buf.try_push(HEX[(b >> 4) as usize] as char)
                .map_err(|_| fmt::Error)?;
            buf.try_push(HEX[(b & 0x0f) as usize] as char)
                .map_err(|_| fmt::Error)?;
        }
        buf.try_push('-').map_err(|_| fmt::Error)?;
        for b in &self.span_id {
            buf.try_push(HEX[(b >> 4) as usize] as char)
                .map_err(|_| fmt::Error)?;
            buf.try_push(HEX[(b & 0x0f) as usize] as char)
                .map_err(|_| fmt::Error)?;
        }
        buf.try_push('-').map_err(|_| fmt::Error)?;
        buf.try_push(HEX[(self.flags >> 4) as usize] as char)
            .map_err(|_| fmt::Error)?;
        buf.try_push(HEX[(self.flags & 0x0f) as usize] as char)
            .map_err(|_| fmt::Error)?;
        Ok(buf)
    }
}

/// OpenTelemetry tracing middleware.
///
/// Extracts W3C `traceparent` from incoming requests, propagates trace context
/// to outbound HTTP calls via task-local, and emits a `tracing` span per request.
///
/// ```rust,ignore
/// router.use_middleware(otel::tracing());
/// ```
pub fn tracing() -> MiddlewareFn {
    Box::new(
        move |req: &Request, next: Next| -> Pin<Box<dyn Future<Output = Response> + Send>> {
            let parent = req.header("traceparent").and_then(parse_traceparent);

            let (trace_id, flags) = match parent {
                Some(p) => (p.trace_id, p.flags),
                None => (random_bytes::<16>(), 0x01),
            };
            let span_id = random_bytes::<8>();

            let ctx = TraceContext {
                trace_id,
                span_id,
                flags,
            };

            let start = std::time::Instant::now();

            let span = ::tracing::info_span!(
                "http_request",
                otel.trace_id = %HexDisplay(&ctx.trace_id),
                otel.span_id = %HexDisplay(&ctx.span_id),
                http.method = req.method(),
                http.path = req.path(),
                http.status = ::tracing::field::Empty,
                latency_ms = ::tracing::field::Empty,
            );

            let handler_fut = next.call(req);
            let record_span = span.clone();

            Box::pin(
                CURRENT_CONTEXT.scope(
                    Some(ctx),
                    async move {
                        let resp = handler_fut.await;
                        record_span.record("http.status", resp.status());
                        record_span.record("latency_ms", start.elapsed().as_millis() as u64);
                        resp
                    }
                    .instrument(span),
                ),
            )
        },
    )
}

/// Initialize the OTLP span exporter. Called from `RuntimeBuilder::run()`
/// when `otel_endpoint()` was configured.
pub(crate) fn init_exporter(endpoint: &str) -> Result<(), crate::RuntimeError> {
    use opentelemetry_otlp::WithExportConfig;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e: opentelemetry_otlp::ExporterBuildError| {
            crate::RuntimeError::Config(e.to_string().into())
        })?;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();

    opentelemetry::global::set_tracer_provider(provider.clone());

    // Install the tracing-opentelemetry bridge layer so that `tracing` spans
    // are forwarded to the OTLP exporter pipeline.
    use opentelemetry::trace::TracerProvider;
    let otel_layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("camber"));
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // Attempt to install as a global subscriber supplement. If a subscriber
    // is already set (e.g., from init_logging), log a warning — the otel
    // layer must be composed during subscriber init instead.
    if let Err(e) = tracing_subscriber::registry().with(otel_layer).try_init() {
        tracing::warn!(
            error = %e,
            "otel tracing layer not installed — a global subscriber is already set"
        );
    }

    let mut guard = PROVIDER.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(provider);
    Ok(())
}

/// Shut down the OTLP tracer provider, flushing pending spans.
pub(crate) fn shutdown_exporter() {
    let provider = {
        let mut guard = PROVIDER.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    match provider.map(|p| p.shutdown()) {
        Some(Err(e)) => tracing::warn!("OTLP tracer provider shutdown failed: {e}"),
        Some(Ok(())) | None => {}
    }
}

static PROVIDER: std::sync::Mutex<Option<opentelemetry_sdk::trace::SdkTracerProvider>> =
    std::sync::Mutex::new(None);

// ── W3C traceparent parsing ──────────────────────────────────────────

/// Parse a W3C `traceparent` header value.
/// Format: `00-{32hex trace_id}-{16hex span_id}-{2hex flags}` = 55 chars.
fn parse_traceparent(value: &str) -> Option<TraceContext> {
    let bytes = value.as_bytes();
    match bytes.len() == 55
        && bytes[0] == b'0'
        && bytes[1] == b'0'
        && bytes[2] == b'-'
        && bytes[35] == b'-'
        && bytes[52] == b'-'
    {
        false => return None,
        true => {}
    }

    let mut trace_id = [0u8; 16];
    hex_decode(&value[3..35], &mut trace_id)?;

    let mut span_id = [0u8; 8];
    hex_decode(&value[36..52], &mut span_id)?;

    let flags = u8::from_str_radix(&value[53..55], 16).ok()?;

    // All-zero trace_id or span_id is invalid per W3C spec
    match trace_id == [0u8; 16] || span_id == [0u8; 8] {
        true => None,
        false => Some(TraceContext {
            trace_id,
            span_id,
            flags,
        }),
    }
}

fn hex_decode(hex: &str, out: &mut [u8]) -> Option<()> {
    match hex.len() == out.len() * 2 {
        false => return None,
        true => {}
    }
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(())
}

/// Convert an ASCII hex digit to its numeric value.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ── Random ID generation ─────────────────────────────────────────────

/// Generate random bytes using the shared thread-local PRNG.
fn random_bytes<const N: usize>() -> [u8; N] {
    crate::prng::random_bytes::<N>()
}

// ── Display helpers ──────────────────────────────────────────────────

struct HexDisplay<'a>(&'a [u8]);

impl fmt::Display for HexDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}
