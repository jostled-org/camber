# Camber Reference

This section is the public reference for Camber's supported library and CLI surface.

Use the reference docs for lookup.
Use the guides for migrations, workflows, and end-to-end setup.

## Start Here

- [Runtime](runtime.md) — runtime model, entrypoints, and structured concurrency
- [HTTP](http.md) — router, handlers, responses, cookies, files, SSE, WebSockets, and host routing
- [Middleware](middleware.md) — middleware model and built-in middleware
- [HTTP Client](client.md) — outbound HTTP client APIs
- [Tasks and Channels](tasks-and-channels.md) — spawn, join, cancellation, channels, and `select!`
- [Errors](error.md) — `RuntimeError` and how failures are categorized
- [Config](config.md) — shared TOML loading and TLS config types
- [TLS](tls.md) — certificate loading, server config, and outbound TLS connections
- [Net](net.md) — listeners, TCP, UDP, TLS streams, and byte forwarding
- [Resources](resource.md) — runtime lifecycle integration for external dependencies
- [Scheduling](schedule.md) — interval and cron-style background work
- [Secrets](secret.md) — loading secrets from environment variables or files
- [Signals and Shutdown](signals.md) — cancellation, shutdown observation, and OS signal wiring
- [Time](time.md) — timeout helpers
- [Logging](logging.md) — tracing subscriber setup helpers
- [Database](database.md) — how Camber fits with `sqlx` and other database layers
- [CLI](cli.md) — `camber new`, `camber serve`, and `camber context`

## Guides

- [Tokio/Axum to Camber](../guides/tokio-to-camber.md)
- [Go to Camber](../guides/go-to-camber.md)
- [Proxy Quickstart](../guides/proxy-quickstart.md)
- [Cross-Compilation](../guides/cross-compile.md)

## Philosophy

- [Vision](../vision.md)
