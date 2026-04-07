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
- no new invocations fire after runtime shutdown begins
- zero-duration intervals are rejected with `RuntimeError::InvalidArgument`

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

## `ScheduleHandle`

All schedulers return `ScheduleHandle`, which is the control surface for cancellation and manual triggering.

For cron schedules, triggering is a no-op; it only affects the interval-based forms.
