# Proxy Quickstart

Config-driven reverse proxy for homelab and internal deployments. Auto-TLS, health checks, host-based routing. No Rust knowledge required.

> **Status:** The proxy is functional for homelab use but is not yet a production edge replacement. Missing features include access logging, rate limiting at the proxy layer, and upstream connection pooling.

This guide is the operator workflow. For field-by-field lookup, use:

- [CLI Reference](../reference/cli.md)
- [Config Reference](../reference/config.md)
- [TLS Reference](../reference/tls.md)

## Install

```sh
cargo install camber-cli
```

## Config

Create `config.toml`. Replace `example.com` with your domain and each `proxy` address with wherever you run that service:

```toml
listen = "0.0.0.0:443"

[tls]
auto = true
email = "admin@example.com"

# Media server
[[site]]
host = "jellyfin.example.com"
proxy = "http://192.168.1.10:8096"

# Photo management
[[site]]
host = "immich.example.com"
proxy = "http://192.168.1.10:2283"

# Cloud storage
[[site]]
host = "nextcloud.example.com"
proxy = "http://192.168.1.10:8080"

# Home automation
[[site]]
host = "homeassistant.example.com"
proxy = "http://192.168.1.10:8123"

# Password manager
[[site]]
host = "vaultwarden.example.com"
proxy = "http://192.168.1.10:8000"
health_check = "/alive"
health_interval = 10

# Uptime monitoring
[[site]]
host = "uptime.example.com"
proxy = "http://192.168.1.10:3001"
health_check = "/api/status-page/heartbeat"

# Dashboards
[[site]]
host = "grafana.example.com"
proxy = "http://192.168.1.10:3000"
health_check = "/api/health"

# Git hosting
[[site]]
host = "gitea.example.com"
proxy = "http://192.168.1.10:3200"

# LLM interface (Open WebUI)
[[site]]
host = "ollama.example.com"
proxy = "http://192.168.1.10:8081"
```

## Run

```sh
camber serve config.toml
```

That's it. Camber provisions TLS certificates for every subdomain, routes by `Host` header, health-checks backends, and returns 503 when a service is down.

## DNS Setup (Cloudflare)

Each subdomain needs a DNS record pointing to your server's public IP. In the Cloudflare dashboard:

1. Go to your domain → **DNS** → **Records**
2. Add an **A record** for each service:

| Type | Name | Content | Proxy status |
|---|---|---|---|
| A | jellyfin | `203.0.113.50` | DNS only |
| A | immich | `203.0.113.50` | DNS only |
| A | nextcloud | `203.0.113.50` | DNS only |
| A | homeassistant | `203.0.113.50` | DNS only |
| A | vaultwarden | `203.0.113.50` | DNS only |
| A | uptime | `203.0.113.50` | DNS only |
| A | grafana | `203.0.113.50` | DNS only |
| A | gitea | `203.0.113.50` | DNS only |
| A | ollama | `203.0.113.50` | DNS only |

Replace `203.0.113.50` with your server's public IP.

**Important:** Set proxy status to **DNS only** (grey cloud), not **Proxied** (orange cloud). Camber needs direct connections for Let's Encrypt ACME challenges to succeed.

**Wildcard alternative:** Instead of one record per service, create a single `*.example.com` A record. All subdomains resolve to your server and Camber routes by hostname. Wildcard DNS works with both auto-TLS (each subdomain gets its own certificate) and manual TLS (use a wildcard cert).

Other DNS providers (Namecheap, Route 53, etc.) work for A record setup — create records pointing each subdomain to your server IP. However, DNS-01 ACME challenges (for servers behind NAT) currently require Cloudflare.

## Config Shape

The practical shape is small:

- top-level `listen`
- optional `connection_limit`
- optional `[tls]`
- one or more `[[site]]` blocks

Each site needs at least one of:

- `proxy`
- `root`

Use the reference docs for the complete field list and validation rules:

- [CLI Reference](../reference/cli.md)
- [Config Reference](../reference/config.md)
- [TLS Reference](../reference/tls.md)

## Connection Limits

Set `connection_limit` to cap the total number of concurrent connections across all sites. Excess connections wait for a slot instead of being dropped. This covers idle keep-alives, SSE, WebSocket, and in-progress TLS handshakes.

```toml
connection_limit = 10000

[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
```

Omit the field for unbounded behavior (the default).

## Proxy + Static File Overlay

When a site has both `proxy` and `root`, Camber serves local files for `GET`/`HEAD` requests and proxies everything else:

```toml
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
root = "/var/www/app/dist"
```

1. `GET /style.css` — if `/var/www/app/dist/style.css` exists, serve it locally
2. `GET /api/users` — no local file, forward to `http://localhost:3000/api/users`
3. `POST /api/users` — always forward to the backend

This is a deterministic local-file overlay: existing local assets win, everything else proxies. It matches the common deployment shape where static assets and SPA shells are served locally while API traffic is proxied.

## Health Checks

When `health_check` is configured, Camber polls `{proxy}{health_check}` at the given interval. If the backend returns a non-2xx status or is unreachable, Camber responds with 503 until the backend recovers.

```toml
[[site]]
host = "grafana.example.com"
proxy = "http://192.168.1.10:3000"
health_check = "/api/health"
health_interval = 5
```

## TLS Modes

### Automatic — Public Server (TLS-ALPN-01)

For servers with a public IP where Let's Encrypt can connect inbound on port 443:

```toml
[tls]
auto = true
email = "admin@example.com"
```

Camber handles certificate provisioning and renewal via ACME TLS-ALPN-01 challenges. Domains are collected from all `host` fields.

### Automatic — Behind NAT (DNS-01, Cloudflare Only)

For servers behind NAT, firewalls, or on private networks where Let's Encrypt cannot reach port 443. Proves domain ownership via DNS TXT records instead. Camber currently supports Cloudflare as the only DNS-01 provider:

```toml
[tls]
auto = true
email = "admin@example.com"
dns_provider = "cloudflare"
dns_api_token_env = "CF_TOKEN"
```

Set the `CF_TOKEN` environment variable to a [Cloudflare API token](https://dash.cloudflare.com/profile/api-tokens) with `Zone:DNS:Edit` permission. Alternatively, store the token in a file:

```toml
[tls]
auto = true
email = "admin@example.com"
dns_provider = "cloudflare"
dns_api_token_file = "/etc/camber/cf.token"
```

Certs are cached to disk and renewed automatically before expiry.

### Manual

```toml
[tls]
cert = "/etc/tls/cert.pem"
key = "/etc/tls/key.pem"
```

### No TLS

Omit the `[tls]` block entirely. Useful behind a load balancer that terminates TLS upstream.

For full TLS field semantics and validation rules, see [TLS Reference](../reference/tls.md).

## Systemd Deployment

Install the binary and config:

```sh
sudo cp target/release/camber /usr/local/bin/
sudo mkdir -p /etc/camber
sudo cp config.toml /etc/camber/config.toml
sudo cp deploy/camber.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now camber
```

Check status:

```sh
sudo systemctl status camber
sudo journalctl -u camber -f
```
