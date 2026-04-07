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

- health checks run periodically on background threads
- shutdown runs during runtime teardown
- resources shut down in reverse registration order

## Design Constraint

`Resource` is synchronous by design.

If your underlying client is async, keep the adapter surface narrow. Do not force async resource lifecycle into this trait unless you actually need it.
