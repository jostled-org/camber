# CLI Reference

The `camber` CLI currently exposes three commands.

## `camber new`

Create a new Camber project from a template.

```sh
camber new my-service --template http
```

Arguments:

- `name` — project directory name
- `--template` — template name, defaults to `http`

## `camber serve`

Run the config-driven reverse proxy.

```sh
camber serve config.toml
```

This is the operator-facing entrypoint for the homelab/internal proxy described in `../guides/proxy-quickstart.md`.

Top-level config fields:

- `listen`
- `connection_limit`
- `[tls]`
- `[[site]]`

`[tls]` fields:

- `cert`, `key` — PEM paths for manual TLS
- `auto` — `true` selects automatic certificates; requires `email`
- `email`, `staging`, `cache_dir` — ACME account contact, Let's Encrypt staging server, certificate cache directory
- `dns_provider` — selects DNS-01; `"cloudflare"` is the only accepted value
- `dns_api_token_env`, `dns_api_token_file` — exactly one of them, with `dns_provider`

Site fields:

- `host` — required
- `proxy` — upstream URL
- `root` — local directory to serve
- `health_check` — upstream path to poll
- `health_interval` — poll interval in seconds

Each site needs at least one of `proxy` and `root`. Both together make a local-file overlay for `GET` and `HEAD`.

With `health_check`, the site has one health state. Camber probes the upstream once before it binds, then every `health_interval` seconds (default 10). While the upstream is unhealthy, each request that would reach it answers 503: proxied methods and overlay `GET` or `HEAD` misses alike. Overlay local files still serve.

### Validation before startup

`camber serve` validates the whole file before it has any effect. A refused file exits nonzero with a diagnostic. It loads no secret, creates no cache, probes no upstream, sends no provider request, and binds no listener.

- The file must declare at least one `[[site]]`. Unknown fields fail, at the top level and in each site.
- `host` is an exact DNS name or an IP address, with an optional port from 1 to 65535. Write IPv6 in brackets: `[::1]:8443`. Wildcards, paths, userinfo, and schemes fail. Names are compared in canonical form: lowercase, with no trailing dot, and IP addresses in standard form. Routing ignores the port. Two sites with the same canonical hostname fail, even if their ports differ. Set `listen` to select the server's listening address and port.
- `proxy` uses `http` or `https` and names an authority. It may carry a path prefix. Credentials, a query, and a fragment fail. The diagnostic does not repeat the URL.
- `health_check` requires `proxy`. It is an absolute path, with no authority and no fragment.
- `health_interval` requires `health_check` and must be at least 1.
- `root` names an existing directory that the process can read.
- Under automatic TLS, each site host must be a DNS name. The certificate names are the site hosts without their ports, each once.

The `[tls]` block refuses unknown fields and fields its mode does not use. `dns_provider` must be `"cloudflare"`. See [TLS Reference](tls.md#acme-domain-names).

`connection_limit = 0` is invalid. So is a limit larger than the admission semaphore can
hold: the server refuses it at start and names that ceiling.

`camber serve --help` lists the same fields and constraints.

## `camber context`

Generate `llms.txt` API context for editor and LLM-assisted workflows.

```sh
camber context
```
