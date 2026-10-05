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

## ACME Domain Names

`AcmeConfig::build()` (TLS-ALPN-01) and `AcmeDns01::provision_cert()` (DNS-01) validate the complete domain set first. `AcmeConfig::validate()` and `AcmeDns01::validate()` run the same checks without an effect. Use them before you load provider credentials or create a cache. A refused set returns `RuntimeError::Config` and causes no credential load, cache write, provider request, or listener bind.

Each name must be a DNS name of ASCII labels:

- 1 to 63 octets per label, 253 octets per name
- letters, digits, and inner hyphens only
- internationalized names as A-labels (`xn--bcher-kva.example`), not U-labels
- one optional trailing dot
- no IP literal: a numeric final label is refused

Camber normalizes each name to lowercase without the trailing dot. Two names that normalize to the same form are a duplicate, and the set is refused. An empty set is refused.

Wildcards differ by challenge:

| Challenge | Wildcard rule | Domain cap |
|---|---|---|
| TLS-ALPN-01 (`AcmeConfig`) | no wildcard | none |
| DNS-01 (`AcmeDns01`) | only a whole leftmost `*.` label, as in `*.example.com` | 100 |

An embedded (`foo.*.example.com`), partial (`w*.example.com`), or bare (`*`) wildcard is refused for both.

## ACME DNS-01 Certificate Cache

`AcmeDns01` caches one certificate generation in its cache directory. A generation is one file, `certificate.pem`: the certificate chain, then the private key.

`AcmeDns01::load_cached_cert()` admits a generation only when all of these are true:

- the leaf and the key parse, and the key belongs to the leaf
- the current time is between the leaf's `notBefore` and `notAfter`
- the leaf's DNS SANs cover every configured domain

Coverage follows TLS hostname rules. A wildcard SAN `*.example.com` covers `api.example.com`. It does not cover `example.com`, `a.b.example.com`, or `badexample.com`. A configured wildcard domain is covered only by the same wildcard SAN.

A refused generation returns `RuntimeError::Integration` with operation `CacheRead` and failure `InvalidCertificate`. It never reaches a `CertStore`. An empty cache returns `Ok(None)`.

`AcmeDns01::needs_renewal()` returns `true` when the leaf's own `notAfter` is less than 30 days away. It also returns `true` when no generation is cached or the cached generation is refused. Camber does not read or write the old `expiry` file.

Camber writes a generation to a private temporary file in the same directory, syncs it, sets mode `0600`, renames it over `certificate.pem`, and syncs the directory. A reader sees the old generation or the new one, never a mix. If a write fails before the rename, the old generation stays and the write returns `CacheWrite`.

### Legacy cache files

Earlier releases wrote `cert.pem` and `key.pem`. Camber reads this pair only when `certificate.pem` does not exist. It validates the pair with the same rules and writes it as `certificate.pem` before it uses it. A corrupt `certificate.pem` is a `CacheRead` error, and Camber does not fall back to the pair. Camber does not delete the old files. The account credentials in `account.json` do not change.

## ACME DNS-01 Providers

Cloudflare is the built-in provider. `CloudflareProvider::new(token)` and `CloudflareProvider::with_base_url(token, base_url)` are synchronous. They validate their inputs and do no I/O. Constructing or dropping a provider sends no request and takes no runtime slot. A base URL must be `https`, or `http` to a loopback host. It must not hold credentials, a query, or a fragment. A refused input returns `RuntimeError::Integration` with operation `ZoneLookup` and failure `InvalidConfig`.

A custom provider implements `DnsProvider`. Its `prepare(&mut self, domains)` callback has no default. Camber calls it with the order's complete canonical domain set before it writes a TXT record. Preparation may read provider metadata. It must not create records or start detached work.

A create that fails must say whether a record can exist. Return `RuntimeError::Integration` with a retryability other than `OutcomeUnknown` only when the provider created nothing. Camber treats any other error, and a panic, as an unknown record.

Cloudflare preparation finds a zone for each domain. It strips a leading `*.` for the lookup only. It selects the longest suffix for which Cloudflare returns exactly one zone of that exact name with a nonempty ID. An empty answer tries the next suffix. Two zones, a zone of another name, a permission error, a malformed body, or a transport error ends preparation. Camber never falls back to a broader zone. The provider publishes its zone map only after every lookup succeeds.

After preparation, the provider refuses a TXT name outside `_acme-challenge.<domain>` for a prepared domain, and a delete of a record it did not create. Each refusal happens before a request is sent. Each request waits at most 10 seconds. A response body over 1 MiB is refused before JSON parsing. A redirect is refused and not followed, so the token never reaches another origin.

## ACME DNS-01 Provisioning

`AcmeDns01::provision_cert(provider)` takes the provider by value. It runs in this order:

1. It validates the whole configuration. A refusal causes no credential load, cache write, or request.
2. It requires a Camber runtime (`NoRuntime` otherwise) and admits one integration entry. A closed runtime returns `ScopeClosed`. A full registry or report budget returns `Busy`. No request is sent.
3. The admitted owner takes the provider and a copy of the configuration. It prepares the provider, runs the order, and cleans up every challenge record it raised.

Neither the provider nor the copy borrows the caller. Dropping the returned future, or the `AcmeDns01` value, asks the order to stop. The owner keeps the provider until the order settles. A stopped or failed preparation writes no TXT record.

| Setter | Default | Rule |
|---|---|---|
| `operation_timeout(d)` | 300 s | the whole order; positive and at most 24 hours |
| `cleanup_timeout(d)` | 5 s | challenge cleanup; positive and at most 24 hours |
| `directory_url(url)` | Let's Encrypt | an absolute `https` URL with a host, no credentials, and no fragment |
| `add_root_certificate(pem)` | platform roots | adds roots for this instance only |

Setters for bounds are checked at admission. `directory_url` and `add_root_certificate` return an error at once. Added roots join the platform roots. They never turn off certificate or hostname checks. Every ACME request waits at most 10 seconds and never past the order's deadline.

The order uses the account in `account.json` only when that account is registered on the configured directory. If the file names a different directory, or no directory, the order registers a new account on the configured directory and replaces the file. Thus a change to `directory_url` or `staging` takes effect at the next order.

ACME retryability depends on the submitted request. Reads, including empty-payload POST-as-GET requests, can fail with `Unavailable` or `Timeout` and remain `Safe`. Explicit rate-limit and bad-nonce refusals are also `Unavailable` and `Safe`, unless the response reports a server failure. Other explicit refusals are `Rejected` and `Never` retryable.

A submitted write whose response is lost or reports a server failure returns `OutcomeUnknown`, never `Safe`. A timeout or cancellation during that write keeps its terminal cause with `OutcomeUnknown` retryability. TXT cleanup cannot resolve an uncertain account, order, challenge, or finalize write at the directory.

`RuntimeBuilder::tls_auto_dns01(acme, token)` only records the configuration. `run` validates the configuration and the token before the runtime starts. Inside the runtime, it admits one owner, prepares every configured zone, and serves the first certificate before the closure runs. A failure ends the run through its normal teardown before anything serves.

## ACME DNS-01 Cleanup

Before each TXT create, the order records the domain. When the provider acknowledges the create, the order records the exact record ID. A create that loses its answer, runs past the order deadline, or is cut by a stop is an unknown record. It can exist in the zone without a known ID.

Cleanup starts after the order's result is fixed. It runs after success, failure, and a stop. It deletes each acknowledged record by its exact ID and never by name, so other records under the same challenge name stay. It does not delete an unknown record.

`cleanup_timeout` bounds the cleanup. A runtime stop shortens it to the runtime's shutdown deadline. A stop does not end cleanup early. When the bound passes, each record whose delete has no answer stays unresolved. Camber never treats elapsed time as a delete.

The order returns only after cleanup settles. If a record stays unresolved, the order fails with `CleanupIncomplete`. `IntegrationError::cleanup()` names each such record once: its domain, its record ID (`None` for an unknown record), and why it stays. A failed order keeps its own failure as the error's source. The account stays charged until shutdown, also after the caller reads it. If a forced stop ends the order before cleanup settles, the runtime's lifecycle failures name every record the order still owed.

Camber serves a new certificate only after cleanup owes nothing and the cache holds the validated generation. A `CleanupIncomplete` or `CacheWrite` failure publishes nothing. Renewal keeps the prior generation and the served certificate. At startup, the run fails before anything serves.

## ACME DNS-01 Renewal

`AcmeDns01::spawn_renewal(provider, store)` returns an `AsyncJoinHandle<Result<(), RuntimeError>>`. It validates the configuration and admits a renewal owner to the current runtime first. A refusal arrives through the handle.

Every 12 hours the owner reads the cached leaf. A leaf whose own `notAfter` is less than 30 days away starts one order. The owner runs one order at a time. It prepares the provider again before each order, and the next order starts only after the prior order's cleanup settles. A failed renewal, including one with incomplete cleanup, keeps the served certificate and the cached generation and retries at the next check. It does not end the task.

Each order reserves a report account before it prepares the provider. When the runtime's report budget is full, the order is `Busy`. It sends no request and adds no retained failure. The owner tries again at the next check.

A failed cleanup stays charged until shutdown. A later successful renewal does not clear it, and the account stays after the owner retires. The runtime's lifecycle failures name each unresolved record once, under the owner that left it.

One runtime renews one cache directory through one owner. While the `tls_auto_dns01` owner or another `spawn_renewal` renews the same cache directory, `spawn_renewal` is refused with `Busy` before any request. Camber names the cache by its absolute path. If the path cannot be made absolute, for example an empty path, the renewal is refused with `InvalidConfig` before any request.

The handle resolves with `Ok(())` when the runtime stops the renewal. `cancel()` asks the owner to stop. An order in progress stops and cleans up its records, then the handle resolves with `Err(Cancelled)`. The root scope keeps the task, so dropping the handle does not detach it.

The runtime-managed `tls_auto_dns01` owner renews through the same loop after its first certificate. It has no handle. Its failed cleanup is reported in the runtime's lifecycle failures.

## ACME DNS-01 Support Boundary

Camber proves its DNS-01 path on one host with no cloud account and no real credential. The proof runs Pebble, the Let's Encrypt test ACME server, with its challenge DNS server, challtestsrv. A loopback test peer answers in the shape of the Cloudflare API and accepts a dummy token. Before it answers a create or a delete, the peer writes the change into challtestsrv. Pebble then validates each challenge against those records. The proof shows that:

- Preparation finds the zone of every domain in a set that spans two zones, before Camber writes a record.
- Camber creates one TXT record for each domain and deletes each one by its exact ID. An unrelated TXT record under the same challenge name stays, in the peer and in DNS.
- The issued chain verifies under the directory's root for every configured domain.
- `tls_auto_dns01` serves the certificate to a client that verifies the hostname. A restart serves `certificate.pem` with no new order.
- A renewal publishes and serves the new certificate only after its cleanup settles.
- After a waiter drop or a runtime stop, Camber deletes each acknowledged record or names it as unresolved.

This is proof of Camber's provider requests and ACME flow. It is not proof of Cloudflare's production behavior, DNS propagation, rate limits, or token permissions, and it does not certify a DNS provider.

## ACME DNS-01 Telemetry

Each DNS-01 operation reports one terminal with the same event, metrics, and labels as the [message-queue integrations](integrations.md#telemetry). The kind is `dns01`. A nested operation reports under the owner's `instance_id` and uses the order's report account. It does not reserve an account of its own.

| Operation | One terminal per |
|---|---|
| `provision` | admitted direct or startup order, including its cleanup |
| `renew` | due renewal order. An idle interval, or a stop between orders, reports none. |
| `zone_lookup` | provider preparation, for the whole domain set. Each zone query is not a terminal. |
| `create_txt` | provider create |
| `delete_txt` | cleanup delete of one exact record ID |
| `cache_read` | logical generation read by an admitted owner. The bundle and the legacy files are one read. |
| `cache_write` | generation publication, including a legacy import |
| `close` | owner settlement |

- A refusal before admission records no duration and names no instance. An invalid configuration is `invalid_config`. `NoRuntime` and `ScopeClosed` are `closed`. A full registry is `busy`. The terminal's operation is the admitting call's: `provision` for `provision_cert` and startup, `renew` for `spawn_renewal`.
- A renewal that the report budget refuses is one `renew` terminal with `busy` and no duration. Its check's `cache_read` comes first.
- A cache that is absent or valid reads as `success`. A refused generation is `invalid_certificate`, and an unreadable file is `unavailable` or `permission_denied`. A publication that fails keeps the prior generation and reports one failed `cache_write`.
- A create whose answer is lost is `outcome_unknown`. A delete that a cleanup bound or a stop cuts short is `cancelled`.
- `create_txt` and `delete_txt` name the exact record ID in the `record_id` event field. A `provision`, `renew`, or forced-stop terminal for an incomplete cleanup names each unresolved record in the `unresolved` event field: its record ID, or its domain if no ID is known. A record ID or a domain is never a metric label. No event carries a TXT value or a token.
- The owner's `close` is `cleanup_incomplete` if any of its orders left a record unresolved. Otherwise it is `success`. The close does not add a second retained account: the order's account names the records.
- `shutdown=true` means the runtime's stop settled the terminal. This occurs for a failure that settles after the stop fired, a nested operation that the stop or a forced stop cut short, and a close that the stop committed. Reading a result again, or moving it into the runtime aggregate, adds no terminal.

`load_cached_cert()` and `needs_renewal()` on `AcmeDns01` do not report terminals, because no admitted owner runs them.

## ALPN

Server TLS configs built by Camber advertise `h2` and `http/1.1`, matching the HTTP server surface Camber supports.
