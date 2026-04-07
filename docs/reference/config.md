# Config Reference

Camber exposes a small shared config layer for TOML-based startup configuration.

## `load_config`

Use `camber::config::load_config(path)` to load and deserialize a TOML file into your own type:

```rust
#[derive(serde::Deserialize)]
struct AppConfig {
    listen: String,
}

let cfg: AppConfig = camber::config::load_config(std::path::Path::new("config.toml"))?;
```

Parse failures are returned as `RuntimeError::Config`.

## `TlsConfig`

`TlsConfig` is the shared TLS block used by Camber's proxy and related tooling.

It supports three public modes:

- manual TLS from PEM cert and key paths
- automatic ACME TLS for publicly reachable servers
- automatic DNS-01 TLS for environments where inbound ACME validation is not possible

The main invariants enforced by `TlsConfig::validate()` are:

- automatic TLS and manual cert/key input are mutually exclusive
- manual TLS requires both cert and key
- automatic TLS requires contact email
- DNS-01 requires a provider plus exactly one token source

## `AcmeBase`

With the `acme` or `dns01` feature enabled, `AcmeBase` holds the shared ACME inputs for both flows: domains, contact email, cache location, and staging choice.

The default cache path is `~/.config/{tool_name}/certs/`.

## Positioning

This module is intentionally small.

- It does not impose an application-wide config schema.
- It does provide the shared TLS schema Camber already knows how to validate.
- The CLI proxy builds on top of it with its own top-level `Config` and `SiteConfig` types.
