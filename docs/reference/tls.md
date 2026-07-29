# TLS Reference

Camber's TLS helpers cover three jobs:

- loading certificates and keys
- building server TLS config
- opening outbound TLS client connections

## Certificate Loading

Use `parse_certified_key(cert_pem, key_pem)` when you already have PEM bytes in memory.

Use `load_certified_key(cert_path, key_path)` when loading from files.

Both return a rustls `CertifiedKey` or `RuntimeError::Tls` on failure.

## `CertStore`

`CertStore` wraps a `CertifiedKey` behind an atomic pointer so new connections can pick up a replacement certificate without restarting the server.

This is the type to use when you want manual certificate hot-swapping.

## Server TLS Resolution

Use `resolve_tls(...)` when your input may be either a prebuilt `CertStore` or PEM file paths and you want Camber to produce the active `rustls::ServerConfig` plus store state.

Use `build_tls_config_from_resolver(...)` when you already have the resolver state and only need the rustls config.

## Outbound TLS Connections

Use `tls::connect(addr, server_name)` for a default client config built from the system root store:

```rust
let mut stream = camber::tls::connect("example.com:443", "example.com").await?;
stream.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n").await?;
```

Use `connect_with(addr, server_name, config)` when you need a custom rustls `ClientConfig`.

Both return `camber::net::TlsStream`.

## ACME DNS-01 Renewal

`AcmeDns01::spawn_renewal(provider, store)` returns a caller-owned `JoinHandle`. It has two completion paths, and neither means renewal failed:

| Completion path | What it means |
|---|---|
| the Camber runtime it was spawned in closes its root scope or latches shutdown | the loop stopped with the runtime that owns it |
| spawned outside a Camber runtime | never — the captured signals are inert and the loop renews forever |

A failed renewal attempt is logged and the loop continues to the next interval, so the handle stays live. The task is caller-owned and is not admitted to the root scope, so the drain does not wait for it.

It panics if called outside a Tokio runtime context.

## ALPN

Server TLS configs built by Camber advertise `h2` and `http/1.1`, matching the HTTP server surface Camber supports.
