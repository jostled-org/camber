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
- manual TLS refuses the automatic-only fields: `email`, `staging`, `cache_dir`, and the DNS fields
- automatic TLS requires contact email
- DNS-01 requires a provider plus exactly one token source
- `dns_provider` must be exactly `"cloudflare"`, the only built-in provider

`validate()` reads no file and no environment variable. Call it before you load the DNS token.

`TlsConfig` refuses unknown fields when it is parsed. Through `load_config`, a misspelled field is a `RuntimeError::Config`, not a silently ignored key.

### `TlsMode`

`TlsConfig::mode()` parses a valid block into the mode it selects. It refuses exactly what `validate()` refuses, with the same diagnostics. Match on the mode. Do not check field combinations again.

- `TlsMode::Manual { cert, key }`: the PEM certificate and private key paths.
- `TlsMode::TlsAlpn(AcmeSettings)`: automatic TLS through ACME TLS-ALPN-01.
- `TlsMode::Dns01 { acme, token }`: automatic TLS through ACME DNS-01 with the Cloudflare provider. `token` is a `SecretRef`, `Env` or `File`.

`AcmeSettings` holds the inputs both automatic modes share: `email`, `staging`, and `cache_dir`. A `cache_dir` of `None` selects the tool's default cache directory.

`mode()` reads no file and no environment variable. Load the token with `camber::secret::load_secret`.

## `canonical_dns_name`

`camber::config::canonical_dns_name(name)` checks one exact DNS host name and returns it in canonical form: ASCII lowercase, with no trailing dot. It refuses a wildcard, a name over 253 octets, an empty label or one over 63 octets, a character outside ASCII letters, digits, and hyphens, a label that begins or ends with a hyphen, and a numeric final label. A refused name is a `RuntimeError::Config`. It performs no I/O. The ACME domain checks use the same label rules.

## `AcmeBase`

With the `acme` or `dns01` feature enabled, `AcmeBase` holds the shared ACME inputs for both flows: domains, contact email, cache location, and staging choice.

The default cache path is `~/.config/{tool_name}/certs/`.

The domain set is validated before any credential load, cache write, provider request, or listener bind. See [ACME Domain Names](tls.md#acme-domain-names).

## Positioning

This module is intentionally small.

- It does not impose an application-wide config schema.
- It does provide the shared TLS schema Camber already knows how to validate.
- The CLI proxy builds on top of it with its own top-level `Config` and `SiteConfig` types. `Config::load` is the only way to build them, and it validates the whole file first. `Config::tls()` returns the parsed `TlsMode`. See [CLI Reference](cli.md#validation-before-startup).
