# Scheduling Reference

Camber provides lightweight interval and cron scheduling on the Tokio runtime.

## Interval Scheduling

Use `schedule::every(interval, f)` for synchronous callbacks:

```rust
use camber::schedule;
use std::time::Duration;

let handle = schedule::every(Duration::from_secs(30), || {
    refresh_cache();
})?;
```

Use `schedule::every_async(interval, f)` for async callbacks:

```rust
let handle = camber::schedule::every_async(std::time::Duration::from_secs(30), || async {
    refresh_cache_async().await;
})?;
```

Behavior is the same across the interval schedulers:

- the first run happens after one full interval
- no new invocations fire after runtime shutdown begins or the root scope closes
- zero-duration intervals are rejected with `RuntimeError::InvalidArgument`
- a callback slower than the interval delays the next wake instead of catching up: missed periods are skipped, never fired back-to-back

`every` takes a synchronous closure and runs it inline on a Tokio worker. It has no await point, so it cannot observe the root scope closing — a slow closure holds its worker until it returns, and one still running at the `shutdown_timeout` escalation boundary makes `runtime::run` return `RuntimeError::ScopeDrainTimeout`. The same applies to `cron`, which is synchronous too.

`every_async` does not make the body stoppable. Only the *wait* between invocations is raced against the lifecycle signals; the callback itself is awaited unguarded, and the drain waits for it. What differs is only what happens to the body at the escalation boundary: an async body is a Tokio task the drain can abort, so it is dropped mid-await rather than left running. `runtime::run` returns `RuntimeError::ScopeDrainTimeout` either way — being outstanding at the boundary is what produces that result, not being unstoppable. Keep the body short in either form.

Every scheduler — interval and cron alike — returns `RuntimeError::NoRuntime` when no runtime context is established. The schedule is refused before any loop is built, never handed back as an inert handle. Once the root scope has closed to admission, schedulers return `RuntimeError::ScopeClosed` for the same reason.

A schedule is a root-scope child. Teardown waits for the loop to exit on the lifecycle signal rather than aborting it outright. A loop still running at the `shutdown_timeout` boundary is aborted there, with every other outstanding async child.

## External Triggering

Use `every_async_notified(interval, notify, f)` when the loop should also wake early from an external `tokio::sync::Notify`.

This is useful when you want both:

- regular polling
- immediate re-run on demand

## Cron Scheduling

Use `schedule::cron(expr, f)` for cron-style callbacks:

```rust
let handle = camber::schedule::cron("*/5 * * * *", || {
    run_job();
})?;
```

Accepted expressions:

- standard 5-field cron expressions
- 6-field or 7-field expressions pass through as-is

For 5-field expressions, Camber prepends a `0` seconds field automatically.

An expression the parser rejects returns `RuntimeError::Schedule` carrying the parser's message. So does an expression that parses but names no occurrence after now — a 7-field form pinned to a past year is the ordinary case. It is refused before any loop is built, rather than handed back as a live handle over a loop that can never fire. An expression that is finite but still ahead cannot be caught at construction: the loop warns and stops when it runs out, and the handle stays live.

Interval schedulers never return `RuntimeError::Schedule`; cron never returns `RuntimeError::InvalidArgument`.

## `ScheduleHandle`

Every scheduler returns `Result<ScheduleHandle, RuntimeError>`. `ScheduleHandle` is the control surface for cancellation and manual triggering.

`cancel()` stops any form and wakes the loop immediately. A cancelled cron schedule exits at once instead of sleeping on to its next occurrence, so it releases its callback and its scope slot right away.

`trigger()` runs the callback early on the interval forms. On a cron schedule it fires no callback — the expression names the occurrences — so it is a no-op there.
