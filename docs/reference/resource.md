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
