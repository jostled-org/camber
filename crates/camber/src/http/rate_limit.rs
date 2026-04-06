use super::middleware::{MiddlewareFn, Next};
use super::request::Request;
use super::response::Response;
use crate::RuntimeError;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

/// Monotonic epoch for nanosecond timestamps.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_ns() -> u64 {
    Instant::now().duration_since(*EPOCH).as_nanos() as u64
}

/// Lock-free token bucket for rate limiting.
struct TokenBucket {
    /// Current available tokens.
    tokens: AtomicU64,
    /// Timestamp (ns since EPOCH) of last refill.
    last_refill_ns: AtomicU64,
    /// Tokens added per interval.
    rate: u64,
    /// Maximum token capacity (burst).
    burst: u64,
    /// Refill interval in nanoseconds.
    interval_ns: u64,
}

impl TokenBucket {
    fn new(rate: u64, interval: Duration, burst: u64) -> Self {
        Self {
            tokens: AtomicU64::new(burst),
            last_refill_ns: AtomicU64::new(now_ns()),
            rate,
            burst,
            interval_ns: interval.as_nanos() as u64,
        }
    }

    /// Attempt to acquire one token. Returns true if granted.
    fn try_acquire(&self) -> bool {
        self.refill();

        loop {
            let current = self.tokens.load(Ordering::Acquire);
            if current == 0 {
                return false;
            }
            match self.tokens.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    /// Refill tokens based on elapsed time.
    ///
    /// The CAS on `last_refill_ns` and the subsequent `add_tokens` CAS are
    /// separate atomic operations. Under high contention a thread can win the
    /// timestamp update but lose the token addition to a concurrent reader,
    /// causing a spurious rejection. This is an acceptable tradeoff for
    /// lock-free operation: the window is sub-microsecond, affects at most one
    /// request per contention event, and the next refill corrects the count.
    fn refill(&self) {
        let now = now_ns();
        loop {
            let last = self.last_refill_ns.load(Ordering::Acquire);
            let elapsed = now.saturating_sub(last);
            let intervals = elapsed / self.interval_ns;
            if intervals == 0 {
                return;
            }
            let new_last = last + intervals * self.interval_ns;
            match self.last_refill_ns.compare_exchange_weak(
                last,
                new_last,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let add = intervals * self.rate;
                    self.add_tokens(add);
                    return;
                }
                Err(_) => continue,
            }
        }
    }

    /// Add tokens up to burst capacity.
    fn add_tokens(&self, add: u64) {
        loop {
            let current = self.tokens.load(Ordering::Acquire);
            let new = (current + add).min(self.burst);
            match self.tokens.compare_exchange_weak(
                current,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(_) => continue,
            }
        }
    }

    /// Estimate seconds until the next token is available.
    fn retry_after_secs(&self) -> u64 {
        let now = now_ns();
        let last = self.last_refill_ns.load(Ordering::Acquire);
        let elapsed = now.saturating_sub(last);
        let remaining_ns = self.interval_ns.saturating_sub(elapsed % self.interval_ns);
        // Ceiling division to seconds (at least 1)
        remaining_ns.div_ceil(1_000_000_000).max(1)
    }
}

fn rate_limit_check(
    bucket: &TokenBucket,
    req: &Request,
    next: Next,
) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    match bucket.try_acquire() {
        true => next.call(req),
        false => {
            let mut buf = itoa::Buffer::new();
            let retry_after: Box<str> = buf.format(bucket.retry_after_secs()).into();
            Box::pin(
                async move { Response::empty_raw(429).with_header("Retry-After", &retry_after) },
            )
        }
    }
}

/// Rate limit to `n` requests per second.
///
/// # Errors
/// Returns `RuntimeError::InvalidArgument` if `n` is zero.
pub fn per_second(n: u64) -> Result<MiddlewareFn, RuntimeError> {
    builder().tokens(n).interval(Duration::from_secs(1)).build()
}

/// Rate limit to `n` requests per minute.
///
/// # Errors
/// Returns `RuntimeError::InvalidArgument` if `n` is zero.
pub fn per_minute(n: u64) -> Result<MiddlewareFn, RuntimeError> {
    builder()
        .tokens(n)
        .interval(Duration::from_secs(60))
        .build()
}

/// Create a rate limit builder for advanced configuration.
pub fn builder() -> RateLimitBuilder {
    RateLimitBuilder {
        tokens: 0,
        interval: Duration::from_secs(1),
        burst: None,
    }
}

/// Builder for customizing rate limit middleware configuration.
pub struct RateLimitBuilder {
    tokens: u64,
    interval: Duration,
    burst: Option<u64>,
}

impl RateLimitBuilder {
    /// Set the number of tokens (requests) allowed per interval.
    pub fn tokens(mut self, n: u64) -> Self {
        self.tokens = n;
        self
    }

    /// Set the refill interval.
    pub fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Set the burst capacity (maximum tokens the bucket can hold).
    ///
    /// Defaults to the token count if not set.
    pub fn burst(mut self, n: u64) -> Self {
        self.burst = Some(n);
        self
    }

    /// Build the rate limiting middleware closure.
    ///
    /// # Errors
    /// Returns `RuntimeError::InvalidArgument` if:
    /// - `tokens` is zero
    /// - `interval` is zero
    /// - `burst` is less than `tokens`
    pub fn build(self) -> Result<MiddlewareFn, RuntimeError> {
        match self.tokens {
            0 => {
                return Err(RuntimeError::InvalidArgument(
                    "rate_limit: tokens must be greater than 0".into(),
                ));
            }
            _ => {}
        }
        match self.interval.is_zero() {
            true => {
                return Err(RuntimeError::InvalidArgument(
                    "rate_limit: interval must be greater than zero".into(),
                ));
            }
            false => {}
        }
        let burst = self.burst.unwrap_or(self.tokens);
        match burst < self.tokens {
            true => {
                return Err(RuntimeError::InvalidArgument(
                    "rate_limit: burst must be >= tokens".into(),
                ));
            }
            false => {}
        }
        let bucket = Arc::new(TokenBucket::new(self.tokens, self.interval, burst));
        Ok(Box::new(move |req, next| {
            rate_limit_check(&bucket, req, next)
        }))
    }
}
