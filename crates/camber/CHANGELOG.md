# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.14.1](https://github.com/jostled-org/camber/compare/camber-v0.14.0...camber-v0.14.1) - 2026-10-22

### Added

- *(http)* let servers move or disable the resource health route

### Fixed

- *(deps)* patch rustls advisories and pin cargo-deny 0.20.2

## [0.14.0](https://github.com/jostled-org/camber/compare/camber-v0.13.2...camber-v0.14.0) - 2026-10-13

### Added

- [**breaking**] implement bounded-outbound-http-streaming

### Other

- *(http)* prove protocol refusals cannot bypass retry budgets

## [0.13.2](https://github.com/jostled-org/camber/compare/camber-v0.13.1...camber-v0.13.2) - 2026-10-11

### Fixed

- *(release)* recover github releases by tag

## [0.13.1](https://github.com/jostled-org/camber/compare/camber-v0.13.0...camber-v0.13.1) - 2026-10-11

### Fixed

- synchronize nats proof and fetch release tags

### Other

- fix public http examples and compile-check them

## [0.13.0](https://github.com/jostled-org/camber/compare/camber-v0.12.0...camber-v0.13.0) - 2026-10-11

### Added

- [**breaking**] implement outbound-http-requests

### Other

- *(http)* make client timeout proof deterministic

### Breaking changes

- *(client)* Outbound HTTP calls fail with `RuntimeError::Client(ClientError)`.
  `ClientError`, `ClientInput`, `ClientTimeout`, `ClientRedirect`, and
  `ClientDecode` are closed enums exported from `camber::http`. Match on the
  category instead of `RuntimeError::Http`, `RuntimeError::Timeout`, or
  `RuntimeError::LimitExceeded(ByteBoundary::ClientResponse)`:
  - a refused connection is `ClientError::Connect`;
  - a DNS failure is `ClientError::Dns`;
  - a TLS failure with a Rustls cause is `ClientError::Tls`;
  - an operation past its total is
    `ClientError::Timeout(ClientTimeout::Operation)`, and an attempt past
    `request_timeout` is `ClientError::Timeout(ClientTimeout::Attempt)`;
  - an expired connect or response-idle timer is
    `ClientError::Timeout(ClientTimeout::Transport)`;
  - a response above its maximum is `ClientError::ResponseTooLarge { limit }`;
  - a client that cannot be constructed is `ClientError::Configuration`;
  - any other send or read failure is `ClientError::Transport`.
- *(client)* Outbound HTTP calls return `ClientResponse`, exported from
  `camber::http`, instead of the server `Response`. This applies to every free
  function and every `Client` verb:
  - replace `resp.body()` with `resp.text()?`, which is strict UTF-8, or with
    `resp.body_bytes()`;
  - `resp.headers()` is an iterator of `(&str, &[u8])` and
    `resp.header(name)` returns the first value. A header value keeps its raw
    bytes, and a non-text value is no longer an empty string;
  - `resp.json::<T>()?` decodes the body without reading `Content-Type`, and
    `resp.into_body()` takes the body as `Bytes`;
  - `resp.error_for_status()?` converts a 400–599 status into
    `ClientError::Status`. A call still returns every status as a response;
  - `ClientResponse` does not implement `IntoResponse`. A handler that
    forwards an upstream answer builds its own `Response`.
- *(client)* The client never decodes a response body. Reqwest's gzip,
  Brotli, deflate, and Zstandard decoding are disabled explicitly, so another
  crate that enables one of those Reqwest features cannot change the returned
  bytes.
- *(client)* `ClientBuilder` is renamed `Client`. `http::client()` returns a
  `Client` that is ready to send, and the old name is not exported. Its other
  configuration methods keep their names. `retry_timeout` and
  `retry_unsafe_methods` are replaced as the entries below describe, and each
  verb method returns `ClientResponse`.
- *(client)* The client does not follow redirects by default. A 3xx answer is
  returned as the response, with its `Location` header and its body, and
  nothing is sent to the place it names. This applies to the free functions
  and to every `Client` that does not set a `RedirectPolicy`. The client sends
  no `Referer` header.
- *(client)* A target is validated before any send. It must be an absolute
  `http` or `https` URL with a host and without a user name or password. Any
  other target fails with
  `ClientError::InvalidRequest(ClientInput::Url)` instead of a transport
  error, and a URL fragment is removed.
- *(client)* `Client::total_timeout` replaces `retry_timeout`. It bounds one
  whole operation: every attempt, every delay between attempts, and the final
  response body. It defaults to 30 seconds and applies to every call:
  - a call with zero retries now has this deadline too. Before, only its
    attempt timers bounded it;
  - an operation past it fails with
    `ClientError::Timeout(ClientTimeout::Operation)` instead of
    `RuntimeError::DeadlineExceeded(DeadlineBoundary::ClientRetry)`. No
    outbound call returns that boundary now;
  - no write to the response budget removes it, including
    `TransferBudget::unbounded()`;
  - the deadline starts when the call is first polled, not when it is made.
- *(client)* The client owns the attempt timer. Reqwest's whole-request
  timeout is no longer set. The timer runs from the send through the end of
  the response body, and an attempt past `request_timeout` fails with
  `ClientError::Timeout(ClientTimeout::Attempt)`. `ClientTimeout::Transport`
  is the connect or response-idle timer only.
- *(client)* Replay permission belongs to one request. `RetryPolicy`, exported
  from `camber::http`, is `Never`, `Idempotent`, or `Replayable`, and
  `ClientRequest::retry_policy` sets it. `ClientBuilder::retry_unsafe_methods`
  is removed:
  - the default is `Idempotent`. With retries configured, `PUT` and `DELETE`
    are now sent again after a transient outcome. Before, only `GET`, `HEAD`,
    and `OPTIONS` were. Use `RetryPolicy::Never` to send such a request once;
  - `POST`, `PATCH`, and every extension method are sent again only under
    `RetryPolicy::Replayable` on that request. The verb conveniences use the
    default policy, so replace `client.retry_unsafe_methods(true).post(..)`
    with `client.request("POST", url)?.retry_policy(RetryPolicy::Replayable)`;
  - `Replayable` permits a retry after a connection that closed or timed out
    before the response head, not only after a connect failure or a transient
    status. No outcome proves the peer did no work, so set it only where a
    duplicate is acceptable;
  - a DNS failure and a TLS failure are never retried, for any method.
- *(client)* Runtime shutdown ends a call with zero retries. A pending attempt
  returns `RuntimeError::Cancelled`. Before, only a call with configured
  retries observed shutdown.
- *(client)* A call polled outside a Tokio executor returns
  `RuntimeError::NoRuntime` and sends nothing.
- *(client)* `connect_timeout` and `backoff` clamp a value above thirty years
  to thirty years, as every other duration setter does.
- *(mock)* A mocked outbound call returns `ClientResponse` and obeys the
  request's response maximum. A mock body above it fails with
  `ClientError::ResponseTooLarge { limit }`. A request is validated before a
  mock answers it. A method-specific mock answers exactly its method, and a
  method-agnostic mock answers any method, including an extension method.
- *(client)* Client errors are redacted. A `ClientError` holds no URL, header
  value, body, or transport error text, and it has no source. A handler that
  returns one answers the fixed `500` internal-service rejection, and the
  operator record carries the category.
- *(client)* The retry debug events name the method label, the host, and the
  failure category. They no longer hold the full URL or the transport error
  text. The new redirect debug event follows the same rule. An extension
  method is recorded as `OTHER`.
- *(client, otel)* A call reads the task's trace context once, at its first
  poll. Every retry and every followed redirect of that call sends the same
  `traceparent`. Before, each send read the context again.

### Added

- *(client)* `RedirectPolicy`, exported from `camber::http`, is `None` or
  `SameOrigin { max_hops }`. `Client::redirect_policy` sets the default for a
  client's requests, and `ClientRequest::redirect_policy` sets it for one
  request. The default is `None`. Under `SameOrigin`:
  - a 301, 302, 303, 307, or 308 answer with one `Location` is followed only
    to the scheme, host, and port the call began at. Another origin fails with
    `ClientError::Redirect(ClientRedirect::OriginChanged)` before anything is
    sent to it. Camber does not remove credentials and send anyway, and it
    does not check the address a permitted host resolves to;
  - a repeated or malformed `Location`, or one with a user name or password,
    fails with `ClientRedirect::InvalidLocation`. An answer with no `Location` is
    returned as the response;
  - a 301 or 302 is followed for `GET` and `HEAD`. A 303 is followed as a
    `GET` with no body and no `Content-Type`, and a `HEAD` stays a `HEAD`. A
    307 or 308 repeats the method and the body for `GET`, `HEAD`, `OPTIONS`,
    `PUT`, and `DELETE`, and for any method under `RetryPolicy::Replayable`.
    Every other case fails with `ClientRedirect::UnsafeMethod`.
    `RetryPolicy::Never` does not stop a redirect;
  - a redirect past `max_hops` fails with `ClientRedirect::HopLimit`. Each
    redirect spends one hop and each retry spends one retry, and neither
    returns the other. A retry repeats the request the last redirect led to,
    not the first request;
  - every followed redirect runs under the same `total_timeout`. A redirect
    answer is released unread before the next send.
- *(client)* `ClientRequest::retries(count)` replaces the client's retry count
  for one request. The default count stays zero.
- *(client)* Each started outbound operation records one `INFO` tracing event
  named `http.client.completed`, when the call returns or when its pending
  future is dropped:
  - the fields are `method`, `host`, `status`, `elapsed_ms`, `attempts`,
    `redirects`, and `outcome`;
  - `outcome` is `response`, `configuration`, `no-runtime`, `invalid-request`,
    `dns`, `connect`, `tls`, `transport`, `timeout`, `redirect`,
    `response-too-large`, `cancelled`, or `abandoned`;
  - `status` is the final response head, `unknown` for a status above 599, or
    `none`. `host` is the host the call began at, and it is in no metric
    label;
  - the event holds no path, query, header value, body, or transport error
    text;
  - a builder refusal and a future that is never polled record no event.
    `text`, `json`, and `error_for_status` record none either.
- *(client, otel)* `ClientRequest::trace_context(enabled)` controls the
  automatic `traceparent` for one request. The default is `true`. `false`
  sends none. A `traceparent` set with `header` is sent unchanged under either
  setting.
- *(client)* `Client::request(method, url)` describes one request and returns
  a `ClientRequest`, exported from `camber::http`. It accepts any HTTP method
  token except `CONNECT`, in the case it is written:
  - `header` and `sensitive_header` set application headers. A name replaces
    its earlier value without regard to case. Transport-owned headers are
    refused. `Authorization`, `Cookie`, and every name set through
    `sensitive_header` stay marked sensitive;
  - `body` replaces the body with bytes, and `json` serializes a value once
    and sets `Content-Type: application/json`;
  - `response_limit` replaces the response maximum for that request only;
  - `deadline` sets an absolute `std::time::Instant` ceiling for that
    request's operation. It can end the operation before `total_timeout` and
    cannot extend it. A deadline already in the past sends nothing;
  - `send` runs the request. Every verb method and free function sends through
    the same description.
- *(client)* A transport setting written to a client that has already sent
  takes effect on its next request. `connect_timeout`,
  `response_idle_timeout`, and `response_budget` discard the cached transport.
- *(client)* Invalid request input fails before any send with
  `ClientError::InvalidRequest(ClientInput)`, and a value that does not
  serialize fails with `ClientError::Encode`.

## [0.12.0](https://github.com/jostled-org/camber/compare/camber-v0.11.1...camber-v0.12.0) - 2026-10-09

### Added

- [**breaking**] implement async-first-websockets

### Fixed

- *(ws)* [**breaking**] prevent upgrade deadlock under reader backpressure
- *(release)* publish with cargo and recover interrupted releases

### Other

- *(macros)* [**breaking**] remove dependency alias support

### Breaking changes

- `#[camber::test]` requires the runtime dependency name `camber`. Dependency aliases are no longer supported.

- *(ws)* `Router::ws` takes a callback that returns a future:
  `F: Fn(&Request, WsConn) -> Fut`, where
  `Fut: Future<Output = Result<(), RuntimeError>> + Send + 'static`. The future
  need not be `Sync` or `Unpin`. Return an `async move` block from each
  callback.
- *(ws)* The waiting WebSocket operations are async. On `WsSender`, await
  `send`, `send_binary`, and `send_shared_binary`. On `WsReceiver`, await
  `recv` and `recv_timeout`. On `WsConn`, await `send`, `send_binary`, `recv`,
  `recv_timeout`, `recv_binary`, and `recv_message`. `WsSender::try_send`,
  `try_send_binary`, and `try_send_shared_binary`, and `WsConn::sender` and
  `split`, stay synchronous.
- *(ws)* A WebSocket callback runs inline in its upgrade task, not on the
  blocking pool. A callback that blocks the thread stops its own connection's
  settlement. Move blocking work to `camber::spawn` and await the result
  through a channel.
- *(ws)* The operator event `camber.websocket.callback.outstanding` is now
  `camber.websocket.callback.cancelled`, and its `disposition` field is
  `cancelled`, not `outstanding-after-forced-grace`. Camber drops a callback
  that is still pending at its settlement deadline, with its captures, before
  the upgrade settles. Change alerts that match the old event name.

### Added

- *(ws)* Current-thread Tokio runtimes serve WebSockets. A wait there no longer
  returns `BlockingInAsyncContext`.
- *(ws)* Dropping a pending send admits nothing and drops the offered payload.
  Dropping a pending `WsReceiver::recv` loses no message. `recv_timeout` spends
  one deadline, and a filtered message does not restart it.

### Fixed

- *(ws)* A forced server stop reports `Finished` only after it has joined each
  aborted WebSocket upgrade task. Before, the server could report `Finished`
  while Tokio had not yet dropped an aborted upgrade task.

## [0.11.1](https://github.com/jostled-org/camber/compare/camber-v0.11.0...camber-v0.11.1) - 2026-10-06

### Other

- order upstream answers after upload starts
- isolate delivery fixtures from host executables

### Breaking changes

- *(mq)* NATS and SQS operations are async only. The `connect_async`,
  `publish_async`, `subscribe_async`, `queue_subscribe_async`,
  `send_message_async`, `receive_messages_async`, and `delete_message_async`
  aliases and the blocking `mq` bridge are removed. Call `connect`, `publish`,
  `subscribe`, `queue_subscribe`, `send_message`, `receive_messages`, and
  `delete_message`, and await them. Configure bounds through `nats::builder`
  and `sqs::builder`.
- *(mq)* A connection captures its Camber runtime when it connects. Outside a
  runtime, connect returns `NoRuntime`. After root admission closes, it returns
  `ScopeClosed`. At 64 live integrations or a full report budget, it returns
  `Busy`.
- *(mq)* `nats::Subscription::next_timeout` returns `Option<Message>`. It
  returns `None` for a completed subscription, not `ChannelClosed`.
  `try_next` on a completed subscription returns `Integration` with `Closed`,
  not `ChannelClosed`.
- *(dns01)* `CloudflareProvider::new(token)` and
  `CloudflareProvider::with_base_url(token, base_url)` take no domain argument.
  They are synchronous and return `Result`. They validate their input and do
  no zone lookup. Remove the domain argument and the `.await` from each call.
- *(dns01)* `RuntimeBuilder::tls_auto_dns01(acme, api_token)` takes no
  domain argument. The runtime prepares the zone of each domain `acme` names.
  Remove the third argument.
- *(dns01)* `DnsProvider` requires `prepare(&mut self, domains)`. Camber calls
  it with the order's whole domain set before it writes a TXT record.
- *(dns01)* `AcmeDns01::provision_cert(provider)` takes the provider by value,
  not by reference. The order owns the provider until its cleanup settles.
- *(dns01)* Provisioning fails when record cleanup is incomplete or the cache
  write fails. A `DnsProvider::create_txt_record` failure must use the
  `OutcomeUnknown` retryability unless nothing was created.
- *(dns01)* `AcmeDns01::spawn_renewal(provider, store)` returns
  `AsyncJoinHandle<Result<(), RuntimeError>>`, not `JoinHandle<()>`. Outside a
  Camber runtime it does not panic: the handle resolves with `NoRuntime`.
- *(errors)* NATS, SQS, and DNS-01 failures are `RuntimeError::Integration`.
  `RuntimeError::MessageQueue` is removed. Read the kind, operation, failure,
  and retryability from the `IntegrationError`.
- *(lifecycle)* `LifecycleParticipant` has a new `Integration { kind, id }`
  variant. An exhaustive `match` must handle it.
- *(config)* `TlsConfig` refuses unknown fields when it is parsed.
  `TlsConfig::validate` refuses fields that the selected TLS mode does not use.
  `dns_provider` must be exactly `"cloudflare"`.

### Added

- *(mq)* `nats::builder` and `sqs::builder` set timeouts, in-flight and
  message-size bounds, and SQS region, endpoint, and credentials.
  `nats::Connection::ready`, `nats::Subscription::next` and `close`,
  `sqs::Client::ready`, and `close` on each handle are new.
- *(dns01)* `AcmeDns01::validate`, `operation_timeout`, `cleanup_timeout`,
  `directory_url`, and `add_root_certificate`.
- *(config)* `camber::config::canonical_dns_name` and `AcmeConfig::validate`.
- *(config)* `TlsConfig::mode` parses a valid block into `TlsMode`: `Manual`,
  `TlsAlpn`, or `Dns01`. `AcmeSettings` holds the inputs both automatic modes
  share.
- *(errors)* `IntegrationError`, `IntegrationKind`, `IntegrationOperation`,
  `IntegrationFailure`, `Retryability`, and `CleanupItem`.

### Fixed

- *(dns01)* Do not report uncertain ACME account, order, challenge, or finalize
  writes as safe to retry. Read-only failures and explicit rate-limit or
  bad-nonce refusals remain safe. Stops retain the submitted write's uncertainty.
- *(dns01)* Verify account persistence through the production writer, including
  cache creation, atomic replacement, private permissions, and typed failures.
- *(grpc)* A streamed RPC's error status reaches the client while the method
  still holds its stream open. Before, the status waited until a download
  bound reset the stream, and the client lost it.
- *(dns01)* The cache stores one `certificate.pem` generation. Camber admits a
  cached or issued generation only when its key matches the leaf, the leaf is
  inside its validity window, and its names cover every configured domain.
  Renewal reads the leaf's own expiry. A valid legacy `cert.pem` and `key.pem`
  pair migrates on read.

## [0.10.1](https://github.com/jostled-org/camber/compare/camber-v0.10.0...camber-v0.10.1) - 2026-09-26

### Fixed

- update dependencies and keep release input records current

## [0.10.0](https://github.com/jostled-org/camber/compare/camber-v0.9.1...camber-v0.10.0) - 2026-09-26

### Fixed

- stabilize compatibility checks and streaming failure tests
- *(multipart)* clear retained-byte observations after cleanup
- [**breaking**] implement adversarial-protocol-correctness

### Other

- *(release)* pass github token to git setup
- *(release)* constrain semver checks to supported features

## [0.9.1](https://github.com/jostled-org/camber/compare/camber-v0.9.0...camber-v0.9.1) - 2026-08-29

### Other

- *(multipart)* stop requiring optional escalation

## [0.9.0](https://github.com/jostled-org/camber/compare/camber-v0.8.1...camber-v0.9.0) - 2026-08-29

### Added

- [**breaking**] implement runtime-simplicity-and-causality

## [0.8.1](https://github.com/jostled-org/camber/compare/camber-v0.8.0...camber-v0.8.1) - 2026-08-21

### Other

- *(websocket)* order cancellation before transport teardown

## [0.8.0](https://github.com/jostled-org/camber/compare/camber-v0.7.0...camber-v0.8.0) - 2026-08-21

### Added

- *(runtime)* [**breaking**] name the outermost failed owner as the primary
- *(runtime)* [**breaking**] share one deadline across shutdown owners
- *(runtime)* [**breaking**] bound every resource callback to one owning coordinator
- *(runtime)* [**breaking**] name the owner every lifecycle failure belongs to
- *(http)* record each request where its operation actually ends
- *(http)* carry each gated chain's stated head onto the real one
- *(codegen)* name the absent wrapper where a skipped one would have been
- *(http)* [**breaking**] bound each gRPC call to the head tonic commits
- *(http)* freeze each proxy route's upstream owner with its graph
- *(http)* bound each streaming direction under its own transfer owner
- *(http)* [**breaking**] retain a rendered profile under a frozen maximum
- *(http)* [**breaking**] read a served file off the worker under a frozen maximum
- *(http)* [**breaking**] collect a buffered proxy answer under a frozen maximum
- *(http)* [**breaking**] collect the client response under a configured maximum
- *(http)* [**breaking**] bind an admitted request to its configured deadlines
- *(http)* [**breaking**] bound service operation with validated policies
- implement shared-immutable-websocket-payloads

### Fixed

- *(http)* shrink internal refusal errors
- *(runtime)* [**breaking**] bound and report the transport edges serving left open
- *(runtime)* align completion proofs with owner order
- *(http)* wake pre-commit operations on control changes
- *(http)* bind middleware, stream heads, and upgrades to the total
- *(http)* observe disconnect before body source failure
- *(test)* retry the blocking serve row on a lost port
- *(http)* refuse policy bounds their owners cannot carry
- *(http)* align service-operation proof boundaries
- *(test)* close checkpoint wake and address-reuse races
- *(ws)* require supervised bridge registration
- *(ws)* [**breaking**] refuse a handshake that declares a payload

### Other

- *(lifecycle)* await WebSocket close handshake
- *(docs)* assert four more reference-page contracts verbatim
- *(runtime)* quote the child's stderr on both isolated failure paths
- *(runtime)* prove a refused readiness pass served no queued peer
- *(runtime)* match every lifecycle name in one place for both roots
- attach the tracing setup summary to the module it describes
- *(runtime)* name every stage a lifecycle failure can be recorded in
- *(http)* read each account's claims off the values production held
- *(http)* read each stated head off the owner that commits it
- *(http)* hold the escalation off the rows a stop would preempt
- *(http)* read the streaming rows' claims off their transfer owners
- *(http)* prove the defaulted profile renders under the frozen maximum
- *(http)* prove the unnamed spellings freeze the documented maximum
- *(http)* prove the frozen maximum is the route's and holds no crossing
- *(http)* prove the response ceiling opt-out, default, and idle order
- *(http)* widen deadline and envelope proofs to every admitted class
- *(ws)* bound the pending connection-permit wait
- *(net)* drop the unreachable limit from the raw accept loop
- *(http)* hold the synchronous rows at their production checkpoints
- *(test)* reuse the runtime's default worker count in tests
- *(test)* retag lifecycle proofs to their current plan steps
- *(http)* cover a server header narrowing the runtime
- *(http)* rename positive_bytes to positive_limit
- *(test)* format included lifecycle test source
- *(http)* cover limit narrowing, policy ordering, and transfer budgets
- *(http)* share the one-transport server setup across permit rows
- *(ws)* keep precedence observation ahead of forced abort
- *(http)* cover gRPC and bare-Tokio connection-limit rows
- *(ws)* order cancellation before terminal polling

## [0.7.0](https://github.com/jostled-org/camber/compare/camber-v0.6.0...camber-v0.7.0) - 2026-08-15

### Added

- [**breaking**] implement independent-websocket-directions

### Fixed

- *(ws)* [**breaking**] make terminal precedence deterministic
- *(ws)* gate carried runtime helper with websocket feature

### Other

- *(ws)* fix equal-ready checkpoint ordering
- *(multipart)* make HTTP/1 backpressure proof deterministic

## [0.6.0](https://github.com/jostled-org/camber/compare/camber-v0.5.2...camber-v0.6.0) - 2026-08-14

### Added

- [**breaking**] implement bounded-streaming-multipart

## [0.5.2](https://github.com/jostled-org/camber/compare/camber-v0.5.1...camber-v0.5.2) - 2026-08-11

### Other

- updated the following local packages: camber-macros

## [0.5.1](https://github.com/jostled-org/camber/compare/camber-v0.5.0...camber-v0.5.1) - 2026-08-11

### Fixed

- *(http)* preserve streaming body refusal precedence

## [0.5.0](https://github.com/jostled-org/camber/compare/camber-v0.4.2...camber-v0.5.0) - 2026-08-10

### Added

- [**breaking**] implement route-aware-body-admission

## [0.4.2](https://github.com/jostled-org/camber/compare/camber-v0.4.1...camber-v0.4.2) - 2026-08-08

### Other

- *(proxy)* read request before scripted response

## [0.4.1](https://github.com/jostled-org/camber/compare/camber-v0.4.0...camber-v0.4.1) - 2026-08-08

### Other

- *(proxy)* synchronize committed stream truncation

## [0.4.0](https://github.com/jostled-org/camber/compare/camber-v0.3.0...camber-v0.4.0) - 2026-08-08

### Added

- [**breaking**] implement structured-framework-rejections

## [0.3.0](https://github.com/jostled-org/camber/compare/camber-v0.2.2...camber-v0.3.0) - 2026-08-02

### Added

- implement raw-request-identity
- [**breaking**] implement runtime-ownership-and-disconnect-cancellation

### Fixed

- [**breaking**] harden runtime ownership and I/O boundaries
- *(http)* await both proxy websocket close replies
- *(release)* decouple workspace package versions

### Other

- implement raw-request-identity-proof-contract
- *(deps)* update rust dependency graph
- fix release-plz and pedant test-tree failures

## [0.2.2](https://github.com/jostled-org/camber/compare/camber-v0.2.1...camber-v0.2.2) - 2026-07-24

### Other

- *(http)* extract shared streaming response builder
- *(http)* split streaming and request recording into modules

## [0.2.1](https://github.com/jostled-org/camber/compare/camber-v0.2.0...camber-v0.2.1) - 2026-07-23

### Other

- repair the container image build
- stop test busy-waits from starving the runner

## [0.2.0](https://github.com/jostled-org/camber/compare/camber-v0.1.8...camber-v0.2.0) - 2026-07-22

### Changed

- Harden runtime shutdown, transport ownership, and lifecycle integration contracts.

### Fixed

- Prevent synchronous runtime loops from missing shutdown wakeups.

## [0.1.8](https://github.com/jostled-org/camber/compare/camber-v0.1.7...camber-v0.1.8) - 2026-07-18

### Added

- *(http)* add owned server lifecycle control

### Fixed

- *(ci)* stabilize warning-clean test suite
- *(ci)* satisfy question-mark lint

### Other

- stabilize pool backpressure coverage
- *(deps)* refresh workspace dependencies

## [0.1.7](https://github.com/jostled-org/camber/compare/camber-v0.1.6...camber-v0.1.7) - 2026-06-07

### Other

- *(deps)* update all dependencies including breaking bumps

## [0.1.6](https://github.com/jostled-org/camber/compare/camber-v0.1.5...camber-v0.1.6) - 2026-04-24

### Added

- *(channel)* add watch channel, supply chain CI, and Tokio boundary docs

## [0.1.5](https://github.com/jostled-org/camber/compare/camber-v0.1.4...camber-v0.1.5) - 2026-04-07

### Fixed

- tighten minimum versions to exclude known vulnerabilities

## [0.1.4](https://github.com/jostled-org/camber/compare/camber-v0.1.3...camber-v0.1.4) - 2026-04-07

### Fixed

- upgrade all breaking dependencies to latest

### Other

- Update README.md

## [0.1.2](https://github.com/jostled-org/camber/compare/camber-v0.1.1...camber-v0.1.2) - 2026-04-07

### Fixed

- update deps
- common/mod.rs fmt
- update rustls deps
