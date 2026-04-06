use super::middleware::Next;
use super::request::Request;
use super::response::Response;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Default methods for CORS preflight responses.
const DEFAULT_METHODS: &str = "GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS";
/// Default max-age for preflight cache (1 hour).
const DEFAULT_MAX_AGE: u32 = 3600;
/// Default allowed headers for CORS preflight responses.
const DEFAULT_HEADERS: &str = "Content-Type, Authorization, Accept";

impl std::fmt::Debug for CorsBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CorsBuilder")
            .field("origin_count", &self.origins.len())
            .field("methods", &self.methods)
            .field("headers", &self.headers)
            .field("max_age", &self.max_age)
            .field("allow_credentials", &self.allow_credentials)
            .finish()
    }
}

/// Builder for customizing CORS middleware configuration.
///
/// Construct via [`builder()`] for fine-grained control, or use
/// [`allow_origins()`] for the common case with sensible defaults.
pub struct CorsBuilder {
    origins: Box<[Box<str>]>,
    methods: Box<str>,
    headers: Box<str>,
    max_age: Box<str>,
    allow_credentials: bool,
}

impl CorsBuilder {
    /// Set allowed origins.
    pub fn origins(mut self, origins: &[&str]) -> Self {
        self.origins = origins.iter().map(|o| Box::from(*o)).collect();
        self
    }

    /// Set allowed HTTP methods.
    pub fn methods(mut self, methods: &[&str]) -> Self {
        self.methods = methods.join(", ").into_boxed_str();
        self
    }

    /// Set allowed request headers.
    pub fn headers(mut self, headers: &[&str]) -> Self {
        self.headers = headers.join(", ").into_boxed_str();
        self
    }

    /// Set the preflight cache duration in seconds.
    pub fn max_age(mut self, seconds: u32) -> Self {
        self.max_age = seconds.to_string().into_boxed_str();
        self
    }

    /// Enable `Access-Control-Allow-Credentials: true`.
    pub fn credentials(mut self) -> Self {
        self.allow_credentials = true;
        self
    }

    /// Build the CORS middleware closure.
    pub fn build(
        self,
    ) -> impl Fn(&Request, Next) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync + 'static
    {
        if self.origins.is_empty() {
            tracing::warn!(
                "CORS middleware built with no allowed origins — all requests will pass through without CORS headers"
            );
        }
        let shared = Arc::new(self);
        move |req, next| cors_middleware(Arc::clone(&shared), req, next)
    }

    /// Check whether `origin` matches any allowed origin.
    /// Returns the resolved Allow-Origin header value:
    /// - `"*"` when a wildcard origin is present and credentials are disabled
    /// - the request origin when matched by wildcard with credentials, or by exact match
    /// - `None` when no origin matches
    fn resolve_origin<'a>(&self, origin: &'a str) -> Option<&'a str> {
        let mut has_wildcard = false;
        let mut exact_match = false;
        for o in self.origins.iter() {
            match o.as_ref() {
                "*" => {
                    has_wildcard = true;
                }
                exact if exact == origin => {
                    exact_match = true;
                }
                _ => {}
            }
        }
        match (has_wildcard, exact_match) {
            (false, false) => None,
            (true, _) if !self.allow_credentials => Some("*"),
            _ => Some(origin),
        }
    }

    fn apply_cors_headers(&self, resp: Response, origin: &str, preflight: bool) -> Response {
        let vary = match preflight {
            true => "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
            false => "Origin",
        };
        let resp = resp
            .with_header("Access-Control-Allow-Origin", origin)
            .with_header("Access-Control-Allow-Methods", &self.methods)
            .with_header("Access-Control-Allow-Headers", &self.headers)
            .with_header("Access-Control-Max-Age", &self.max_age)
            .with_header("Vary", vary);
        match self.allow_credentials {
            true => resp.with_header("Access-Control-Allow-Credentials", "true"),
            false => resp,
        }
    }
}

fn is_preflight(req: &Request) -> bool {
    req.method() == "OPTIONS" && req.header("access-control-request-method").is_some()
}

fn cors_middleware(
    config: Arc<CorsBuilder>,
    req: &Request,
    next: Next,
) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    let origin_ref = match req.header("origin") {
        Some(o) => o,
        None => return next.call(req),
    };

    let header_origin = match config.resolve_origin(origin_ref) {
        Some(o) => o.to_owned(),
        None => return next.call(req),
    };

    let preflight = is_preflight(req);
    match preflight {
        true => {
            let resp = config.apply_cors_headers(Response::empty_raw(204), &header_origin, true);
            Box::pin(async move { resp })
        }
        false => {
            let resp_fut = next.call(req);
            Box::pin(async move {
                let resp = resp_fut.await;
                config.apply_cors_headers(resp, &header_origin, false)
            })
        }
    }
}

/// Create CORS middleware that allows the specified origins.
///
/// Handles preflight OPTIONS requests automatically and adds
/// CORS headers to responses for matching origins.
pub fn allow_origins(
    origins: &[&str],
) -> impl Fn(&Request, Next) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync + 'static
{
    builder().origins(origins).build()
}

/// Create a CORS builder for customizing allowed origins, methods, headers, and max-age.
pub fn builder() -> CorsBuilder {
    CorsBuilder {
        origins: Box::default(),
        methods: DEFAULT_METHODS.into(),
        headers: DEFAULT_HEADERS.into(),
        max_age: DEFAULT_MAX_AGE.to_string().into_boxed_str(),
        allow_credentials: false,
    }
}
