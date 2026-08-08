use super::Request;
use super::middleware::{MiddlewareFn, MiddlewareFuture, Next};
use arrayvec::ArrayString;
use std::fmt;
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
    ///
    /// Infallible, and proven so by `TRACEPARENT_LEN`: the buffer holds exactly
    /// what the four fixed-width pieces render to, so no push can overflow it.
    /// The former fallible form swallowed its error into an empty string, and
    /// the caller injects this value without checking it — an empty
    /// `traceparent` header on every outbound request.
    pub(crate) fn format_traceparent(self) -> ArrayString<TRACEPARENT_LEN> {
        let mut buf = ArrayString::new();
        buf.push_str("00-");
        push_hex(&mut buf, &self.trace_id);
        buf.push('-');
        push_hex(&mut buf, &self.span_id);
        buf.push('-');
        push_hex(&mut buf, &[self.flags]);
        buf
    }
}

/// The rendered width of a W3C `traceparent`, stated as the pieces it is built
/// from. A field that changes width fails the build below rather than the push.
const TRACEPARENT_LEN: usize = 55;

const _: () = assert!(TRACEPARENT_LEN == 3 + 16 * 2 + 1 + 8 * 2 + 1 + 2);

/// Append each byte as two lowercase hexadecimal digits.
fn push_hex<const N: usize>(buf: &mut ArrayString<N>, bytes: &[u8]) {
    const HEX: [u8; 16] = *b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| {
            [
                HEX[(b >> 4) as usize] as char,
                HEX[(b & 0x0f) as usize] as char,
            ]
        })
        .for_each(|digit| buf.push(digit));
}

/// OpenTelemetry tracing middleware.
///
/// Extracts W3C `traceparent` from incoming requests, propagates trace context
/// to outbound HTTP calls via task-local, and emits a `tracing` span per request.
///
/// The span names the request, not its answer. A middleware frame sees the
/// response its own chain returned, and the rejection boundary can still
/// displace that response with the fixed fallback after the chain unwinds, so a
/// status recorded here would claim a status the peer never received. The
/// completion event at the wire exit owns the status and the latency; this span
/// carries the `request_id` both sides are joined on.
///
/// ```rust,ignore
/// router.use_middleware(otel::tracing());
/// ```
pub fn tracing() -> MiddlewareFn {
    Box::new(move |req: &Request, next: Next| -> MiddlewareFuture {
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

        let span = ::tracing::info_span!(
            "http_request",
            // The key every other per-request emit site is named by: the
            // completion event, the rejection record, and the built-in header
            // all carry this value, so a trace joins to all three.
            request_id = %req.request_id(),
            otel.trace_id = %HexDisplay(&ctx.trace_id),
            otel.span_id = %HexDisplay(&ctx.span_id),
            // The bounded label, never the token the peer sent: a method
            // Camber cannot route on still reaches this span, and a span
            // field taken from an arbitrary token is an unbounded label.
            http.method = req.method_label(),
            http.path = req.path(),
        );

        let handler_fut = next.call(req);

        Box::pin(CURRENT_CONTEXT.scope(
            Some(ctx),
            async move { Ok(handler_fut.await) }.instrument(span),
        ))
    })
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

    // Install the tracing-opentelemetry bridge layer so that `tracing` spans
    // are forwarded to the OTLP exporter pipeline.
    use opentelemetry::trace::TracerProvider;
    let otel_layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("camber"));
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // A subscriber already set — by `init_logging`, or by anything else that
    // claimed the global — leaves nothing forwarding spans into the batch
    // processor this function just started. Warning about that gave the process
    // an exporter that received nothing and flushed nothing at shutdown, for
    // its whole life, which is the same fault `with_metrics` refuses at
    // startup. Refused here for the same reason, and the provider is shut down
    // first so the refusal leaves no processor behind.
    if let Err(error) = tracing_subscriber::registry().with(otel_layer).try_init() {
        shutdown_after_refusal(&provider);
        return Err(crate::RuntimeError::Config(
            format!(
                "otel_endpoint set, but a global tracing subscriber is already \
                 installed ({error}), so no span would reach the exporter. Compose \
                 the otel layer during subscriber setup, or drop the init_logging \
                 call that runs before RuntimeBuilder::run."
            )
            .into_boxed_str(),
        ));
    }

    opentelemetry::global::set_tracer_provider(provider.clone());

    let mut guard = PROVIDER.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(provider);
    Ok(())
}

/// Stop the batch processor of a provider this function is about to refuse.
///
/// The provider never reached the global slot or `PROVIDER`, so nothing else
/// will ever take it back. Its own shutdown failure cannot displace the refusal
/// being reported, so it is warned rather than returned.
fn shutdown_after_refusal(provider: &opentelemetry_sdk::trace::SdkTracerProvider) {
    match provider.shutdown() {
        Err(error) => tracing::warn!(
            %error,
            "OTLP tracer provider shutdown failed while refusing otel startup"
        ),
        Ok(()) => {}
    }
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
