# Logging Reference

Camber's logging helpers are thin wrappers around `tracing` and `tracing-subscriber`.

## `init_logging`

Use `camber::logging::init_logging(format, level)` to install a global tracing subscriber.

```rust
use camber::logging::{self, LogFormat, LogLevel};

logging::init_logging(LogFormat::Text, LogLevel::Info);
```

If a global subscriber is already installed, this call becomes a no-op.

### With `otel_endpoint`

Do not call `init_logging` before a runtime that sets `otel_endpoint`. The subscriber
this call installs claims the global slot, and the OTLP exporter needs that slot to
forward spans. `RuntimeBuilder::run` refuses the startup with `RuntimeError::Config`
rather than run an exporter no span reaches.

To export spans, drop the `init_logging` call, or install your own subscriber stack
with an OTLP layer composed into it.

## Output Shape

Camber keeps the choice small:

- `LogFormat::Text` for human-readable local output
- `LogFormat::Json` for structured ingestion

Verbosity runs from `Error` through `Trace`.

## Integration Telemetry

NATS, SQS, and DNS-01 report operation results through `tracing` and `metrics`.
Each terminal result emits one `INFO` event: `integration operation finished`.
It increments `camber_integration_operations_total`; admitted operations also
record `camber_integration_operation_duration_seconds`.

Metric labels use only `kind`, `operation`, and `outcome`. Payloads and
credentials appear in neither events nor labels. Reading a result again or
moving it into the runtime's lifecycle report emits no additional terminal.

See [Integration Telemetry](integrations.md#telemetry) for event fields and
per-operation rules, and [DNS-01 Telemetry](tls.md#acme-dns-01-telemetry) for
nested operations and cleanup records. `init_logging` installs only the tracing
subscriber; it does not install a metrics recorder.

## Scope

This module only installs the subscriber.

- It does not wrap the `tracing` macros.
- It does not provide file rotation or log shipping.
- It is a convenience for common service setup.

If your application already installs its own subscriber stack, use that directly.
