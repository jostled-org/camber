use super::Response;
use super::map_reqwest_error;
use super::method::Method as LocalMethod;
use super::mock;
use crate::RuntimeError;
use crate::runtime;
use reqwest::Method;
use std::borrow::Cow;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

/// Generate async HTTP method wrappers on `ClientBuilder`.
/// Two arms: `no_body` for methods without a request body,
/// and a content-type literal for methods that take `body: &str`.
macro_rules! http_methods {
    ($($(#[$meta:meta])* $name:ident => $method:ident, no_body;)*) => {
        $(
            $(#[$meta])*
            pub async fn $name(&self, url: &str) -> Result<Response, RuntimeError> {
                self.send(Method::$method, url, None).await
            }
        )*
    };
    ($($(#[$meta:meta])* $name:ident => $method:ident, $ct:literal;)*) => {
        $(
            $(#[$meta])*
            pub async fn $name(&self, url: &str, body: &str) -> Result<Response, RuntimeError> {
                self.send(Method::$method, url, Some(($ct, body))).await
            }
        )*
    };
    ($($(#[$meta:meta])* $name:ident => $method:ident, $kind:tt;)*) => {
        http_methods! { @split [] [] [$($(#[$meta])* $name => $method, $kind;)*] }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] []) => {
        http_methods! { $($nb)* }
        http_methods! { $($wb)* }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] [$(#[$meta:meta])* $name:ident => $method:ident, no_body; $($rest:tt)*]) => {
        http_methods! { @split [$($nb)* $(#[$meta])* $name => $method, no_body;] [$($wb)*] [$($rest)*] }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] [$(#[$meta:meta])* $name:ident => $method:ident, $ct:literal; $($rest:tt)*]) => {
        http_methods! { @split [$($nb)*] [$($wb)* $(#[$meta])* $name => $method, $ct;] [$($rest)*] }
    };
}

/// Generate free-standing async HTTP functions that delegate to `default_dispatch`.
macro_rules! http_free_functions {
    ($($(#[$meta:meta])* $name:ident => $method:ident, no_body;)*) => {
        $(
            $(#[$meta])*
            pub async fn $name(url: &str) -> Result<Response, RuntimeError> {
                default_dispatch(Method::$method, url, None).await
            }
        )*
    };
    ($($(#[$meta:meta])* $name:ident => $method:ident, $ct:literal;)*) => {
        $(
            $(#[$meta])*
            pub async fn $name(url: &str, body: &str) -> Result<Response, RuntimeError> {
                default_dispatch(Method::$method, url, Some(($ct, body))).await
            }
        )*
    };
    ($($(#[$meta:meta])* $name:ident => $method:ident, $kind:tt;)*) => {
        http_free_functions! { @split [] [] [$($(#[$meta])* $name => $method, $kind;)*] }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] []) => {
        http_free_functions! { $($nb)* }
        http_free_functions! { $($wb)* }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] [$(#[$meta:meta])* $name:ident => $method:ident, no_body; $($rest:tt)*]) => {
        http_free_functions! { @split [$($nb)* $(#[$meta])* $name => $method, no_body;] [$($wb)*] [$($rest)*] }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] [$(#[$meta:meta])* $name:ident => $method:ident, $ct:literal; $($rest:tt)*]) => {
        http_free_functions! { @split [$($nb)*] [$($wb)* $(#[$meta])* $name => $method, $ct;] [$($rest)*] }
    };
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_BACKOFF: Duration = Duration::from_millis(100);

static CLIENT: LazyLock<Result<reqwest::Client, Arc<str>>> = LazyLock::new(|| {
    build_client(DEFAULT_TIMEOUT, DEFAULT_TIMEOUT).map_err(|e| -> Arc<str> { e.to_string().into() })
});

fn default_client() -> Result<&'static reqwest::Client, RuntimeError> {
    CLIENT
        .as_ref()
        .map_err(|e| RuntimeError::Http(Arc::clone(e)))
}

fn build_client(
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<reqwest::Client, RuntimeError> {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(read_timeout)
        .build()
        .map_err(|e| RuntimeError::Http(e.to_string().into()))
}

fn client_build_error(err: RuntimeError) -> Arc<str> {
    match err {
        RuntimeError::Http(msg) => msg,
        other => other.to_string().into(),
    }
}

async fn build_and_send(
    client: &reqwest::Client,
    method: &Method,
    url: &str,
    body: Option<(&str, &str)>,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut builder = client.request(method.clone(), url);
    if let Some((content_type, payload)) = body {
        // reqwest::Body only implements From<&'static str> and From<String>,
        // so non-static &str requires an owned copy per attempt.
        builder = builder
            .header("content-type", content_type)
            .body(String::from(payload));
    }
    #[cfg(feature = "otel")]
    if let Some(ctx) = super::otel::current_context() {
        builder = builder.header("traceparent", ctx.format_traceparent().as_str());
    }
    builder.send().await
}

fn is_transient_status(status: u16) -> bool {
    matches!(status, 429 | 502..=504)
}

fn jitter_nanos() -> u64 {
    crate::prng::next_u64()
}

fn exponential_jitter(base: Duration, attempt: u32) -> Duration {
    let multiplier = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let exp = base.saturating_mul(multiplier);
    let jitter_bound = base.as_nanos().min(u128::from(u64::MAX)) as u64;
    let jitter = match jitter_bound {
        0 => Duration::ZERO,
        n => Duration::from_nanos(jitter_nanos() % n),
    };
    exp.saturating_add(jitter)
}

async fn sleep_backoff(base: Duration, attempt: u32, override_secs: Option<u64>) {
    let delay = match override_secs {
        Some(secs) => Duration::from_secs(secs),
        None => exponential_jitter(base, attempt),
    };
    tokio::time::sleep(delay).await;
}

/// Parse a `Retry-After` header value as whole seconds.
fn parse_retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
}

async fn do_request_with_retry(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    body: Option<(&str, &str)>,
    retries: u32,
    backoff: Duration,
) -> Result<Response, RuntimeError> {
    let mut remaining = retries;
    loop {
        let result = build_and_send(client, &method, url, body).await;

        let (is_transient, retry_after) = match &result {
            Ok(resp) => (
                is_transient_status(resp.status().as_u16()),
                parse_retry_after(resp),
            ),
            Err(e) => (e.is_timeout() || e.is_connect(), None),
        };

        let attempt = retries - remaining;
        match (remaining > 0 && is_transient, result) {
            (true, Ok(ref resp)) => {
                tracing::debug!(
                    method = %method,
                    url = url,
                    status = resp.status().as_u16(),
                    attempt = attempt + 1,
                    "retrying transient HTTP status"
                );
                sleep_backoff(backoff, attempt, retry_after).await;
                remaining -= 1;
            }
            (true, Err(ref e)) => {
                tracing::debug!(
                    method = %method,
                    url = url,
                    error = %e,
                    attempt = attempt + 1,
                    "retrying transient HTTP error"
                );
                sleep_backoff(backoff, attempt, retry_after).await;
                remaining -= 1;
            }
            (false, Ok(resp)) => return read_response(resp).await,
            (false, Err(e)) => return Err(map_reqwest_error(e)),
        }
    }
}

fn try_mock(method: &Method, url: &str) -> Option<Response> {
    let local_method = LocalMethod::from_reqwest(method);
    local_method.and_then(|m| mock::try_intercept(m, url))
}

async fn retry_dispatch(
    client: &reqwest::Client,
    method: Method,
    url: &str,
    body: Option<(&str, &str)>,
    retries: u32,
    backoff: Duration,
) -> Result<Response, RuntimeError> {
    runtime::check_cancel()?;
    match try_mock(&method, url) {
        Some(resp) => Ok(resp),
        None => do_request_with_retry(client, method, url, body, retries, backoff).await,
    }
}

/// Create a client builder with custom timeout and retry configuration.
pub fn client() -> ClientBuilder {
    ClientBuilder {
        connect_timeout: DEFAULT_TIMEOUT,
        read_timeout: DEFAULT_TIMEOUT,
        retries: 0,
        backoff: DEFAULT_BACKOFF,
        cached_client: OnceLock::new(),
    }
}

impl std::fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .field("retries", &self.retries)
            .field("backoff", &self.backoff)
            .finish()
    }
}

/// Builder for configuring outbound HTTP client timeouts and retry behavior.
///
/// The underlying `reqwest::Client` is built lazily on first request
/// and cached for subsequent calls.
pub struct ClientBuilder {
    connect_timeout: Duration,
    read_timeout: Duration,
    retries: u32,
    backoff: Duration,
    cached_client: OnceLock<Result<reqwest::Client, Arc<str>>>,
}

impl ClientBuilder {
    /// Set the connect timeout. Minimum: 1ms. Zero values are clamped.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        const MIN: Duration = Duration::from_millis(1);
        self.connect_timeout = crate::time::clamp_duration(timeout, MIN, "connect_timeout");
        self
    }

    /// Set the read timeout (applies to both response headers and body).
    /// Minimum: 1ms. Zero values are clamped.
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        const MIN: Duration = Duration::from_millis(1);
        self.read_timeout = crate::time::clamp_duration(timeout, MIN, "read_timeout");
        self
    }

    /// Set the maximum number of retries for transient errors.
    /// Transient errors: connection failures, timeouts, 502/503/504 responses.
    pub fn retries(mut self, n: u32) -> Self {
        self.retries = n;
        self
    }

    /// Set the base backoff duration between retries.
    /// Actual delay: `base * 2^attempt + jitter`. Minimum: 1ms. Zero values are clamped.
    pub fn backoff(mut self, duration: Duration) -> Self {
        const MIN: Duration = Duration::from_millis(1);
        self.backoff = crate::time::clamp_duration(duration, MIN, "backoff");
        self
    }

    fn get_client(&self) -> Result<&reqwest::Client, RuntimeError> {
        self.cached_client
            .get_or_init(|| {
                build_client(self.connect_timeout, self.read_timeout).map_err(client_build_error)
            })
            .as_ref()
            .map_err(|e| RuntimeError::Http(Arc::clone(e)))
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<(&str, &str)>,
    ) -> Result<Response, RuntimeError> {
        retry_dispatch(
            self.get_client()?,
            method,
            url,
            body,
            self.retries,
            self.backoff,
        )
        .await
    }

    http_methods! {
        /// Send an HTTP GET request with the configured timeouts.
        get => GET, no_body;
        /// Send an HTTP POST request with a text body using the configured timeouts.
        post => POST, "text/plain";
        /// Send an HTTP POST request with a JSON body using the configured timeouts.
        post_json => POST, "application/json";
        /// Send an HTTP POST request with a URL-encoded form body using the configured timeouts.
        post_form => POST, "application/x-www-form-urlencoded";
        /// Send an HTTP PUT request with a text body using the configured timeouts.
        put => PUT, "text/plain";
        /// Send an HTTP PUT request with a JSON body using the configured timeouts.
        put_json => PUT, "application/json";
        /// Send an HTTP PUT request with a URL-encoded form body using the configured timeouts.
        put_form => PUT, "application/x-www-form-urlencoded";
        /// Send an HTTP DELETE request with the configured timeouts.
        delete => DELETE, no_body;
        /// Send an HTTP DELETE request with a text body using the configured timeouts.
        delete_with_body => DELETE, "text/plain";
        /// Send an HTTP PATCH request with a text body using the configured timeouts.
        patch => PATCH, "text/plain";
        /// Send an HTTP PATCH request with a JSON body using the configured timeouts.
        patch_json => PATCH, "application/json";
        /// Send an HTTP PATCH request with a URL-encoded form body using the configured timeouts.
        patch_form => PATCH, "application/x-www-form-urlencoded";
        /// Send an HTTP HEAD request with the configured timeouts.
        head => HEAD, no_body;
        /// Send an HTTP OPTIONS request with the configured timeouts.
        options => OPTIONS, no_body;
    }
}

/// Async dispatch using the shared default client (no retries).
async fn default_dispatch(
    method: Method,
    url: &str,
    body: Option<(&str, &str)>,
) -> Result<Response, RuntimeError> {
    retry_dispatch(default_client()?, method, url, body, 0, Duration::ZERO).await
}

http_free_functions! {
    /// Send an HTTP GET request with default 30s timeouts.
    get => GET, no_body;
    /// Send an HTTP POST request with a text body and default 30s timeouts.
    post => POST, "text/plain";
    /// Send an HTTP POST request with a JSON body and default 30s timeouts.
    post_json => POST, "application/json";
    /// Send an HTTP POST request with a URL-encoded form body and default 30s timeouts.
    post_form => POST, "application/x-www-form-urlencoded";
    /// Send an HTTP PUT request with a text body and default 30s timeouts.
    put => PUT, "text/plain";
    /// Send an HTTP PUT request with a JSON body and default 30s timeouts.
    put_json => PUT, "application/json";
    /// Send an HTTP PUT request with a URL-encoded form body and default 30s timeouts.
    put_form => PUT, "application/x-www-form-urlencoded";
    /// Send an HTTP DELETE request with default 30s timeouts.
    delete => DELETE, no_body;
    /// Send an HTTP DELETE request with a text body and default 30s timeouts.
    delete_with_body => DELETE, "text/plain";
    /// Send an HTTP PATCH request with a text body and default 30s timeouts.
    patch => PATCH, "text/plain";
    /// Send an HTTP PATCH request with a JSON body and default 30s timeouts.
    patch_json => PATCH, "application/json";
    /// Send an HTTP PATCH request with a URL-encoded form body and default 30s timeouts.
    patch_form => PATCH, "application/x-www-form-urlencoded";
    /// Send an HTTP HEAD request with default 30s timeouts.
    head => HEAD, no_body;
    /// Send an HTTP OPTIONS request with default 30s timeouts.
    options => OPTIONS, no_body;
}

async fn read_response(resp: reqwest::Response) -> Result<Response, RuntimeError> {
    let status = resp.status().as_u16();

    let headers: Vec<_> = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            let name: Cow<'static, str> = Cow::Owned(k.as_str().to_owned());
            let value: Cow<'static, str> = Cow::Owned(v.to_str().unwrap_or("").to_owned());
            (name, value)
        })
        .collect();

    let body_bytes = resp
        .bytes()
        .await
        .map_err(|e| RuntimeError::Http(e.to_string().into()))?;

    Ok(Response::new(status, body_bytes, headers))
}
