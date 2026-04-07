# Logging Reference

Camber's logging helpers are thin wrappers around `tracing` and `tracing-subscriber`.

## `init_logging`

Use `camber::logging::init_logging(format, level)` to install a global tracing subscriber.

```rust
use camber::logging::{self, LogFormat, LogLevel};

logging::init_logging(LogFormat::Text, LogLevel::Info);
```

If a global subscriber is already installed, this call becomes a no-op.

## Output Shape

Camber keeps the choice small:

- `LogFormat::Text` for human-readable local output
- `LogFormat::Json` for structured ingestion

Verbosity runs from `Error` through `Trace`.

## Scope

This module only installs the subscriber.

- It does not wrap the `tracing` macros.
- It does not provide file rotation or log shipping.
- It is a convenience for common service setup.

If your application already installs its own subscriber stack, use that directly.
