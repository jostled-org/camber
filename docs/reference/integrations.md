# Integrations

Camber's optional integrations are concrete handles owned by the runtime that
created them. This page states what each one supports and what its success
means.

## Support Matrix

| Integration | Feature | Supported | Not supported |
| --- | --- | --- | --- |
| Core NATS | `nats` | Connect, publish, subscribe, queue groups, receive, close; opt-in publishing acknowledged by an existing JetStream stream | JetStream stream administration, consumers, replay, and deduplication; request/reply helpers; application headers; exactly-once delivery |
| Amazon SQS | `sqs` | Connect, queue readiness, send, receive, delete, close on Standard queues | FIFO groups and deduplication, batch APIs, queue administration, exactly-once delivery |
| ACME DNS-01 | `dns01` | The built-in Cloudflare provider, or a custom provider through the `DnsProvider` trait; multi-zone preparation, issuance, cache, renewal, cleanup by exact record ID | Other built-in providers, cleanup by record name, any claim about a provider's production behavior |
| Native gRPC | `grpc` | tonic services in the unary, client-streaming, server-streaming, and bidirectional forms, beside HTTP routes | A runtime-owned handle, a status Camber maps after tonic commits its head |

Each feature compiles alone and with the others. CI compiles the library and
every example on this page with no optional feature, with each feature alone,
and with all of them.

The `camber serve` reverse proxy uses the `dns01` provider for its DNS-01
mode. The proxy stays a homelab and internal tool, still in progress. It is
not a production edge replacement. See the [CLI reference](cli.md).

## Common Contract

A managed integration captures its Camber runtime once, when it connects.

- Outside a Camber runtime, connect returns `NoRuntime`. After root admission
  closes, it returns `ScopeClosed`. At 64 live integrations, or with the
  runtime's 256 integration report accounts in use, it returns `Busy`. None of
  these refusals performs I/O.
- A builder validates its whole configuration before it creates an SDK client.
  Durations must be positive and at most 24 hours. Counts and byte maximums
  must be positive and at most the Tokio semaphore permit limit. An invalid
  value returns `InvalidConfig`.
- Each operation is admitted before Camber copies its input. At the operation
  limit or a full report budget, it returns `Busy` and sends nothing.
- Close is idempotent. It commits before it returns, and every clone of the
  handle refuses new work after that. Every closer reads the same result.
- Dropping the last handle requests close. The runtime owns the close until it
  settles.
- A failure no caller read, a cleanup failure, and a failed or incomplete
  close stay in the runtime's report accounts. The runtime returns them in its
  lifecycle aggregate. See [Runtime](runtime.md#managed-integrations) and
  [Errors](error.md).
- Retained failures are never evicted. When they fill all 256 accounts, every
  new connect, operation, and DNS-01 order returns `Busy` until the runtime
  ends. Work already admitted still settles. See
  [Report Budget and `Busy`](error.md#report-budget-and-busy).

Failures are `RuntimeError::Integration`. Their `retryability()` is `Safe` only
when a repeat cannot apply the operation twice: nothing was sent, the service
refused the request before it took effect, or the operation only reads. A lost
result after submission is `OutcomeUnknown`: the peer can have the command.
Camber never retries or replays an operation.

## Telemetry

NATS, SQS, and DNS-01 use one shared terminal vocabulary. A terminal is one
`INFO` event with the fixed message `integration operation finished`, one
increment of `camber_integration_operations_total`, and at most one sample of
`camber_integration_operation_duration_seconds`. Each section below tells when
one of its operations reports a terminal. DNS-01 is described in the
[TLS reference](tls.md#acme-dns-01-telemetry).

| Label | Values |
| --- | --- |
| `kind` | `nats`, `sqs`, `dns01` |
| `operation` | `connect`, `ready`, `publish`, `subscribe`, `receive`, `delete`, `close`, `zone_lookup`, `create_txt`, `delete_txt`, `provision`, `cache_read`, `cache_write`, `renew` |
| `outcome` | `success`, or a failure: `invalid_config`, `unavailable`, `permission_denied`, `rejected`, `busy`, `limit_exceeded`, `timeout`, `cancelled`, `closed`, `outcome_unknown`, `cleanup_incomplete`, `invalid_certificate` |

`camber_integration_operations_total` and
`camber_integration_operation_duration_seconds` carry these three labels and no
others. The terminal event also carries `shutdown`, plus `failure` and
`retryability` (`never`, `safe`, or `outcome_unknown`) on a failure, and
`instance_id` after admission. A DNS-01 record operation adds `record_id`. A
DNS-01 terminal that leaves cleanup records unresolved adds `unresolved`. These
are event fields, never labels. No URL, subject, SDK message, credential, or
payload is a label or an event field. The outcome is `success` or the failure's
fixed name.

## Core NATS

```rust,no_run
use camber::mq::nats;
use std::time::Duration;

async fn notify() -> Result<(), camber::RuntimeError> {
    let connection = nats::builder("nats://127.0.0.1:4222")
        .operation_timeout(Duration::from_secs(5))
        .max_subscriptions(8)
        .connect()
        .await?;
    let mut orders = connection.subscribe("orders.created").await?;
    connection.publish("orders.created", b"order 7").await?;
    if let Some(message) = orders.next().await? {
        println!("{}", String::from_utf8_lossy(message.payload()));
    }
    connection.close().await
}
```

`nats::connect(url)` uses the defaults. `nats::builder(url)` sets:

| Setter | Default | Bound |
| --- | --- | --- |
| `connect_timeout` | 10 s | The handshake and the readiness flush |
| `operation_timeout` | 30 s | One publish or subscribe, SDK queuing included |
| `shutdown_timeout` | 5 s | The local close, narrowed by the runtime's shutdown deadline |
| `max_in_flight` | 64 | Publish and subscribe operations running at once |
| `max_message_bytes` | 1 MiB | Each published and each delivered payload |
| `subscription_capacity` | 64 | Messages the SDK buffers per subscription |
| `client_capacity` | 64 | Commands the SDK queues for its connection |
| `max_subscriptions` | 64 | Subscriptions open at once |

What each result means:

- **Connect** succeeds after the server answered the handshake and the SDK
  flushed the connection.
- **`Connection::ready`** reports the local state now: `Closed` after close,
  `Unavailable` while the SDK is disconnected. It does not promise that the
  next publish succeeds.
- **Publish** succeeds after the SDK flushed the message to its socket.
  This is a local flush, not a server receipt, subscriber processing, or
  durable storage. For a server receipt, see
  [Acknowledged Publishing](#acknowledged-publishing). A payload over `max_message_bytes` returns
  `LimitExceeded` before Camber copies it. While the SDK is disconnected,
  publish returns `Unavailable`; Camber keeps no offline queue. If you drop a
  publish after the SDK queued the message, the SDK still sends it: the
  runtime aggregate keeps a `Cancelled` publish with an unknown outcome.
- **Subscribe** and **queue_subscribe** succeed after the SDK flushed the
  subscription. At `max_subscriptions` they return `Busy`. Closing a
  subscription returns its slot. If you drop a subscribe before it returns,
  the SDK withdraws any registration it queued, and no account remains.
- **`Subscription::next`** waits for a message. `None` means the subscription
  completed; it never means a timeout. **`next_timeout`** returns `Timeout`
  when its bound passes. **`try_next`** never waits: `None` means nothing is
  buffered, and a completed subscription returns `Closed`. A delivered payload
  over `max_message_bytes` returns `LimitExceeded` and is dropped; the
  subscription stays open.
- **Close** waits for running operations, then for the SDK's closed event,
  within `shutdown_timeout`. Without that event it returns `Timeout`: an
  incomplete close, never a success.

Core NATS can drop messages for a subscriber that cannot keep up, and the SDK
can drop its own overflow notification. Camber therefore detects no loss it
was not told about. When the SDK does deliver a slow-consumer notification,
Camber closes the whole connection and every subscription, because the SDK
names no public subscription for the event. That close reports
`LimitExceeded` for `Receive`, and the runtime aggregate keeps it.

Reconnection belongs to the SDK. Camber adds no reconnect loop and never
republishes a message whose flush it did not see.

### Acknowledged Publishing

`acknowledged_publishing(stream)` makes each publish wait for the server to
acknowledge storing the message in an existing JetStream stream. Subscribe,
receive, and close keep their Core meaning.

```rust,no_run
use camber::mq::nats;

async fn record(payload: &[u8]) -> Result<(), camber::RuntimeError> {
    let connection = nats::builder("nats://127.0.0.1:4222")
        .acknowledged_publishing("EVENTS")
        .connect()
        .await?;
    connection.publish("events.created", payload).await?;
    connection.close().await
}
```

- **Stream name.** 1 to 255 ASCII letters, digits, `_`, or `-`. Case is kept.
  Any other name returns `InvalidConfig` from `connect`, before the runtime
  check and before any I/O. A later call replaces an earlier one. A builder
  without the call, and `nats::connect(url)`, publish Core.
- **The stream must exist.** It must already capture the published subjects.
  Camber never looks it up, creates it, changes it, or deletes it. Connect
  succeeds without it; each publish then fails. A missing stream never falls
  back to Core success.
- **Permissions.** Publish on the data subjects, and subscribe on the
  private inbox subtree (`_INBOX.>` by default). No stream administration
  permission is needed.
- **Connect** also subscribes one private wildcard inbox before readiness.
  It is one extra SDK subscription that buffers up to
  `subscription_capacity` replies. It takes no `max_subscriptions` slot.
- **Publish** sends the message with the `Nats-Expected-Stream` header and a
  private reply subject. It succeeds only when the server's reply names the
  configured stream with a positive sequence: the server accepted the message
  under that stream's storage configuration. That is not disk durability
  beyond the stream's configuration, subscriber processing, or exactly-once
  delivery. One `operation_timeout` covers the SDK queue and the wait for the
  acknowledgement. Acknowledged publishes and subscribes share
  `max_in_flight`.

What an acknowledged publish returns:

| Evidence | Failure | Retryability |
| --- | --- | --- |
| Invalid subject | `Rejected` | `Never`, nothing sent |
| Payload over `max_message_bytes`, or over the server maximum with the header | `LimitExceeded` | `Never`, nothing sent |
| `max_in_flight` or the report budget full | `Busy` | `Safe`, nothing sent |
| Disconnected, or the SDK refused to queue it | `Unavailable` | `Safe`, nothing sent |
| The reply receiver ended before the publish registered | `Unavailable` | `Safe`, nothing sent |
| The connection has no reply token left | `LimitExceeded` | `Never`, nothing sent |
| No responders (status 503) | `Unavailable` | `Safe` |
| Typed stream mismatch | `Rejected` | `Never` |
| Typed server message or header size refusal | `LimitExceeded` | `Never` |
| Typed permission refusal (code 403) | `PermissionDenied` | `Never` |
| Any other JetStream error response | `Rejected` | `Never` |
| Deadline before the SDK queued it | `Timeout` | `Safe` |
| Deadline after the SDK queued it | `Timeout` | `OutcomeUnknown` |
| Disconnect after the SDK queued it; a reply that is malformed, over 4096 bytes, for another stream, with sequence zero, or with another status; or the reply receiver ended after the publish registered | `OutcomeUnknown` | `OutcomeUnknown` |

A permissions error the server reports for the whole connection is not a
reply to one publish. A publish already queued then stays unknown until its
own reply, a disconnect, or its deadline. Camber never republishes. A
duplicate or late reply changes no result.

Camber decodes at most 4096 bytes of a reply. That bounds Camber's own
parsing, not the frames the SDK allocates. `subscription_capacity` bounds the
number of queued replies, not their bytes.

### NATS Telemetry

Each NATS operation reports one [terminal](#telemetry).

- A refusal before admission records no duration. Configuration errors,
  `NoRuntime`, `ScopeClosed`, `Busy`, an outbound payload over the maximum,
  every `Closed` refusal, and an `Unavailable` refusal while disconnected are
  refusals. A `NoRuntime` or `ScopeClosed` refusal is one `connect` terminal
  with no instance and the outcome `closed`.
- A refusal from the SDK's queue is an admitted failure and records a
  duration. So is every failure of an acknowledged publish after admission:
  a registration refusal, such as an ended reply receiver or no reply token
  left, and every receipt failure.
- An inbound payload over the maximum is an admitted `receive` failure and
  records a duration.
- An admitted operation records its duration from admission to its committed
  result. A connect is admitted when the runtime admits the connection.
- Every receive call is one terminal: a message, an empty `try_next`, and a
  completed stream are each a `success`.
- The first close of a connection or a subscription is one terminal. A
  repeated close read adds none. A connection whose connect failed reports no
  close.
- A delivered slow-consumer event reports one `receive` terminal with
  `limit_exceeded`, and no close terminal.
- `shutdown=true` means the runtime's stop settled the terminal: the stop
  committed the close, or a forced stop dropped the work. Work that a forced
  stop drops reports one terminal with an unknown outcome.
- Reading a result again, or moving it into the runtime aggregate, adds no
  terminal. A publish dropped after it was submitted reports one `cancelled`
  terminal with an unknown outcome.

## Amazon SQS

```rust,no_run
use camber::mq::sqs;
use std::time::Duration;

async fn drain_one(queue: &str) -> Result<(), camber::RuntimeError> {
    let client = sqs::builder()
        .region("us-east-1")
        .operation_timeout(Duration::from_secs(25))
        .connect()
        .await?;
    client.ready(queue).await?;
    client.send_message(queue, "order 7").await?;
    for message in client.receive_messages(queue, 10, Duration::from_secs(20)).await? {
        if let Some(receipt) = message.receipt_handle() {
            client.delete_message(queue, receipt).await?;
        }
    }
    client.close().await
}
```

`sqs::connect()` uses the defaults. `sqs::builder()` sets:

| Setter | Default | Bound |
| --- | --- | --- |
| `connect_timeout` | 10 s | Configuration and credential loading |
| `operation_timeout` | 30 s | One request, from admission to its answer |
| `shutdown_timeout` | 5 s | How long a close waits for running operations |
| `max_in_flight` | 64 | Operations running at once |
| `max_message_bytes` | 1 MiB | Each sent and each delivered body |
| `region` | The SDK's region chain | A nonempty run of letters, digits, and hyphens |
| `endpoint` | The service endpoint | An absolute `http` or `https` URL without userinfo or query |
| `credentials` | The SDK's credential chain | Nonempty keys, for this client alone |

Explicit credentials never change the process environment. When the region or
the credentials are omitted, the SDK's chains load them within
`connect_timeout`. With both given, Camber reads no ambient configuration.

What each result means:

- **Connect** loads configuration and credentials. It sends no request and
  makes no readiness claim.
- **`Client::ready(queue_url)`** succeeds after one queue query. It proves the
  queue answered this client at that instant, not that send, receive, or
  delete is permitted. A missing queue (`Rejected`) and a denial
  (`PermissionDenied`) are `Never`; repeating the query cannot fix them. A
  throttle, a lost answer, and a timeout are `Safe`, because the query is
  read-only.
- **Send** returns the message ID the service acknowledged. A body over
  `max_message_bytes` returns `LimitExceeded` before Camber copies it. An
  answer without a message ID returns `OutcomeUnknown`.
- **Receive** asks for 1 to 10 messages and waits at most 20 seconds; other
  values return `Rejected` before any request. Each receive is one request, and
  Camber runs no polling loop. A batch larger than asked for, or with a body
  over `max_message_bytes`, returns `LimitExceeded`: no message reaches the
  caller, none is deleted, and each returns to the queue when its visibility
  timeout ends.
- **Delete** succeeds when the service acknowledged the receipt. A stale or
  invalid receipt returns `Rejected`.
- **Close** lets running operations finish within `shutdown_timeout`. An
  operation still running then returns `Cancelled`, and its caller owns that
  failure. If forced runtime shutdown cannot settle an operation, close
  returns `Timeout` with `Never` retryability. The runtime retains that close
  failure. Concurrent and later callers read the same fixed result.

Camber sends each request once: it sets the SDK to one attempt and adds no
retry. A service answer is typed by its error code first, because SQS answers
some throttles and denials with status 400. A throttle code
(`RequestThrottled`, `ThrottlingException`, `Throttling`, `KmsThrottled`,
`OverLimit`) is `Unavailable` and `Safe`: the service refused the request
before it took effect. An access-denial code is `PermissionDenied`. A
server-failure code (`InternalError`, `InternalFailure`, `ServiceUnavailable`)
is `Unavailable`. When Camber does not recognize the code, the status decides:
401 and 403 are `PermissionDenied`, another 4xx is `Rejected`, and a 5xx is
`Unavailable`.

A connection that was never established is `Unavailable` and `Safe`. A timeout
or a cut before the request reached the transport is `Safe` too, and a waiter
dropped then leaves no account. For send, receive, and delete, a server
failure has `OutcomeUnknown` retryability. So do a lost answer, a timeout, and
a cut after the request reached the transport. Each of the three operations
can have taken effect, because a receive hides its messages. A dropped waiter
after submission leaves that unknown outcome in the runtime's report accounts.

A local emulator such as ElasticMQ proves these Camber contracts. It is not
evidence for AWS IAM, regional behavior, or service availability.

### SQS Telemetry

Each SQS operation reports one [terminal](#telemetry). No queue URL, body, or
receipt handle is a label or an event field.

- One `connect`, `ready`, `publish`, `receive`, or `delete` call is one
  terminal. A receive is one terminal for its whole batch, not one per
  message. Camber sends one request per call, so the SDK adds no attempt.
- A refusal before admission records no duration. Configuration errors,
  `NoRuntime`, `ScopeClosed`, `Busy`, a body over the maximum, receive
  parameters out of range, and every `Closed` refusal are refusals. A
  `NoRuntime` or `ScopeClosed` refusal is one `connect` terminal with no
  instance and the outcome `closed`.
- An admitted operation records its duration from admission to its
  committed result. A connect is admitted when the runtime admits the client.
  A batch over its bounds and an unreadable answer are admitted `receive`
  failures.
- A loaded client reports one close terminal. A repeated or concurrent close
  read adds none, and a client whose connect failed reports no close. If the
  runtime's stop commits the close, the close terminal has `shutdown=true`.
- An operation that close cuts reports one `cancelled` terminal. Its outcome is
  unknown only when the request reached the transport.
- A waiter dropped after submission reports one `cancelled` terminal with an
  unknown outcome. The runtime aggregate keeps that account and adds no
  terminal.

## ACME DNS-01

```rust,no_run
use camber::dns01::{AcmeDns01, CloudflareProvider};
use camber::{CertStore, RuntimeError};

async fn issue_and_renew(token: &str) -> Result<(), RuntimeError> {
    let issuer = AcmeDns01::new("camber", ["example.com", "*.example.com"])
        .email("admin@example.com")
        .cache_dir("/var/lib/camber/acme");
    let provider = CloudflareProvider::new(token.into())?;
    let certificate = issuer.provision_cert(provider).await?;
    let store = CertStore::new(certificate);
    let renewals = CloudflareProvider::new(token.into())?;
    issuer.spawn_renewal(renewals, store).await?
}
```

Provider construction is pure. `CloudflareProvider::new` and
`CloudflareProvider::with_base_url` validate their input and return at once:
no zone lookup, no request, and no runtime slot. A custom provider implements
`DnsProvider`, including `prepare`. An order takes its provider by value and
owns it until the order's cleanup settles. Preparation, provisioning, cleanup,
renewal, and the support boundary are in the
[TLS reference](tls.md#acme-dns-01-providers).

## Native gRPC

```rust,no_run
use camber::http::{GrpcRouter, Router};

fn mount() -> Router {
    let (_reporter, health) = tonic_health::server::health_reporter();
    let mut router = Router::new();
    router.grpc(GrpcRouter::new().add_service(health));
    router
}
```

The example uses the `tonic-health` crate. Camber does not re-export it, so
add `tonic-health` 0.14 to your own dependencies.

The `grpc` feature serves tonic services through Camber's HTTP/2 server. It is
not a runtime-owned integration handle, so the common contract above does not
apply to it. All four native forms (unary, client-streaming, server-streaming,
and bidirectional) use the same middleware gate, budgets, and completion
record as other requests.

Camber maps a transfer bound only before tonic's head is committed. After the
head, tonic owns the status, and a download bound resets only its own stream.
[gRPC transfer bounds](http.md#grpc-transfer-bounds) gives the tested
boundary for each form and direction.

A peer reset, `cancel()`, a graceful `shutdown()`, and an expired aggregate
deadline settle every form with one completion and no mapped rejection. A
reset ends only its own stream.
[gRPC cancellation and shutdown](http.md#grpc-cancellation-and-shutdown) gives
the tested boundary for each phase.
