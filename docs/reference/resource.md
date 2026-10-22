# Resource Reference

`Resource` integrates long-lived dependencies into Camber's runtime lifecycle.

Typical examples:

- caches
- database pools
- queue clients
- service connectors that need health checks and shutdown hooks

## Trait Shape

```rust
use camber::{Resource, RuntimeError};

struct Cache;

impl Resource for Cache {
    fn name(&self) -> &str { "cache" }
    fn health_check(&self) -> Result<(), RuntimeError> { Ok(()) }
    fn shutdown(&self) -> Result<(), RuntimeError> { Ok(()) }
}
```

The trait has three responsibilities:

- provide a stable name for health and logging
- answer a synchronous health check
- perform synchronous shutdown work

## Runtime Integration

Register resources with `runtime::builder().resource(...)`:

```rust
camber::runtime::builder()
    .resource(Cache)
    .run(|| Ok::<(), camber::RuntimeError>(()))?;
```

Behavior:

- one readiness health check runs before the service admits traffic; any
  failure refuses the run and returns `RuntimeError::Lifecycle`
- later health checks run on the configured interval, in registration order
- shutdown runs during runtime teardown, in reverse registration order
- one resource never has two callbacks running at once
- every callback runs on its own worker under the deadline `ResourceBudget`
  configures for its phase
- shutdown starts after every managed integration has settled or been named
  outstanding in the lifecycle aggregate

Managed integrations are not resources. They have their own runtime owner and
never use this callback registry.

## Budgets And Ownership

`ResourceBudget` carries three finite deadlines — startup health, periodic
health, and shutdown — and defaults to 30 seconds each:

```rust
use camber::{ResourceBudget, runtime};
use std::time::Duration;

# fn main() -> Result<(), camber::RuntimeError> {
let budget = ResourceBudget::bounded(
    Duration::from_secs(5),
    Duration::from_secs(2),
    Duration::from_secs(10),
)?;
runtime::builder().resource_budget(budget).run(|| ())?;
# Ok(())
# }
```

The deadline bounds how long Camber **waits**, not how long the callback runs. A
synchronous callback has no cancellation point, so a worker that misses its
deadline keeps the resource until the callback returns on its own. Camber
records the resource as having exceeded its deadline, refuses to start another
callback for it, and never claims the worker was terminated. A later phase that
finds the resource still held reports it as blocked rather than calling a second
callback concurrently.

## Health Projection

`/health` reports every registered resource, in registration order:

```json
{
  "status": "unhealthy",
  "resources": {
    "db": { "status": "ok" },
    "cache": { "status": "error", "failure": "returned" }
  }
}
```

`failure` is one of `returned`, `panicked`, `deadline`, `lost-worker`, or
`blocked`. The cause behind a failure never reaches this body — it is carried in
the structured operator event the coordinator emits, and no resource name
becomes a metric label. The endpoint answers 200 when every resource is well and
503 otherwise.

### Which server serves the report

The runtime owns the registered resources and runs every health check. Each
server decides whether it answers the report, and at which path, through
`ServerPolicy::health_route`:

| `HealthRoute` | The server |
| --- | --- |
| `Default` | answers at `/health`, or at the path the runtime's policy chose |
| `Path("/ops/health")` | answers at that exact path |
| `Disabled` | answers no report |

A service that must keep its own `/health` contract disables Camber's route on
that server and registers its own handler. One runtime can expose the report on
an admin listener and hide it on a public one:

```rust
use camber::http::{self, HealthRoute, ServerPolicy};

let admin = ServerPolicy::default().health_route(HealthRoute::Path("/ops/health"))?;
let public = ServerPolicy::default().health_route(HealthRoute::Disabled)?;
http::server(admin_router).policy(admin).serve_background(admin_listener)?;
http::server(public_router).policy(public).serve_background(public_listener)?;
```

Health checks, their operator events, and resource shutdown run whatever the
servers choose. A server answers the report only when the runtime has at least
one resource.

A `Path` must start with `/`, name only static segments, and carry no empty
segment, trailing `/`, query, or fragment. It cannot be `/metrics` or
`/debug/pprof/cpu`. `health_route` refuses any other path with
`RuntimeError::InvalidArgument`.

Inside a runtime, the server's choice narrows the runtime's
(`RuntimeBuilder::server_policy`). `Disabled` on either side disables the
report. A server's `Path` wins over the runtime's choice. A server's `Default`
keeps the runtime's choice.

Camber matches its internal routes before the router. So while a server answers
the report, a router route at the same exact path could never answer. The
server refuses to start with `RuntimeError::InvalidArgument` naming that path.
The same rule refuses a router `/metrics` while metrics are on, and a router
`/debug/pprof/cpu` while profiling is on.

Only exact static routes are refused. A parameter or wildcard route that also matches the path is not refused. Examples
are `/:page`, a root proxy, and root static files. Such a route still answers
every other path.

## Failures

No resource failure is reduced to a log line:

- a readiness failure prevents the service from serving and is returned from
  `RuntimeBuilder::run`
- a periodic failure is published through `/health` and one operator event
- a teardown failure is retained in the `RuntimeError::Lifecycle` aggregate the
  runtime returns, and never stops another resource's teardown from being
  attempted

Read an aggregate through `iter()` and `len()`.

## Subprocess Lifecycle

`Resource` integrates child processes into runtime shutdown. Wrap a process handle in a
`Resource` impl so Camber kills it during teardown:

```rust
use std::process::{Child, Command};
use camber::{Resource, RuntimeError};

struct LspServer {
    child: std::sync::Mutex<Option<Child>>,
}

impl LspServer {
    fn start(bin: &str) -> Result<Self, RuntimeError> {
        let child = Command::new(bin)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?; // io::Error converts to RuntimeError::Io via From
        Ok(Self {
            child: std::sync::Mutex::new(Some(child)),
        })
    }
}

impl Resource for LspServer {
    fn name(&self) -> &str { "lsp-server" }

    fn health_check(&self) -> Result<(), RuntimeError> {
        let guard = self.child.lock().unwrap();
        match &*guard {
            Some(_) => Ok(()),
            None => Err(RuntimeError::InvalidArgument("lsp process not running".into())),
        }
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        let mut guard = self.child.lock().unwrap();
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(())
    }
}
```

Register the subprocess resource at startup:

```rust
let lsp = LspServer::start("/usr/bin/my-lsp")?;

camber::runtime::builder()
    .resource(lsp)
    .run(|| { /* ... */ })?;
```

For async subprocess IO (reading stdout, writing stdin), use `tokio::process::Command` and
Tokio's async IO traits directly. Camber does not wrap these — they are protocol-specific
and below the service abstraction layer. Spawn reader/writer tasks with `camber::spawn_async`
so they participate in structured concurrency and shut down with the runtime.

## Design Constraint

`Resource` is synchronous by design.

If your underlying client is async, keep the adapter surface narrow. Do not force async resource lifecycle into this trait unless you actually need it.
