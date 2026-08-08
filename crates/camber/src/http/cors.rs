use super::method::Method;
use super::middleware::{Next, ResponseFuture};
use super::request::Request;
use super::response::Response;
use std::borrow::Cow;
use std::sync::Arc;

/// Default methods for CORS preflight responses.
const DEFAULT_METHODS: &str = "GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS";
/// Default max-age for preflight cache (1 hour), written as the header value it
/// becomes. The number was only ever rendered to build this text, and a default
/// held as text is a constant every response borrows instead of copies.
const DEFAULT_MAX_AGE: &str = "3600";
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
///
/// The three header values are `Cow` because a default configuration never
/// changes them: the builder stores the constants borrowed, so a CORS response
/// hands them on without copying. Only a caller that sets its own values pays
/// for them, and only once, at build time.
pub struct CorsBuilder {
    origins: Box<[Box<str>]>,
    methods: Cow<'static, str>,
    headers: Cow<'static, str>,
    max_age: Cow<'static, str>,
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
        self.methods = Cow::Owned(methods.join(", "));
        self
    }

    /// Set allowed request headers.
    pub fn headers(mut self, headers: &[&str]) -> Self {
        self.headers = Cow::Owned(headers.join(", "));
        self
    }

    /// Set the preflight cache duration in seconds.
    pub fn max_age(mut self, seconds: u32) -> Self {
        self.max_age = Cow::Owned(seconds.to_string());
        self
    }

    /// Enable `Access-Control-Allow-Credentials: true`.
    pub fn credentials(mut self) -> Self {
        self.allow_credentials = true;
        self
    }

    /// Build the CORS middleware closure.
    pub fn build(self) -> impl Fn(&Request, Next) -> ResponseFuture + Send + Sync + 'static {
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
    ///
    /// The wildcard answer is a constant, so it is borrowed. Returning a borrow
    /// of the request instead made the caller copy `"*"` onto the heap on every
    /// response — and `allow_origins(&["*"])` without credentials is the common
    /// configuration.
    fn resolve_origin(&self, origin: &str) -> Option<Cow<'static, str>> {
        let has_wildcard = self.origins.iter().any(|allowed| allowed.as_ref() == "*");
        let exact_match = self
            .origins
            .iter()
            .any(|allowed| allowed.as_ref() == origin);
        match (has_wildcard, exact_match) {
            (false, false) => None,
            (true, _) if !self.allow_credentials => Some(Cow::Borrowed("*")),
            _ => Some(Cow::Owned(origin.to_owned())),
        }
    }

    /// Add this configuration's CORS headers to one response.
    ///
    /// Every name here is a literal, and the `Vary` value is one of two fixed
    /// sentences, so they are borrowed rather than copied: these headers ride on
    /// every CORS response, and a copy per response is the allocation
    /// `HeaderPair` exists to avoid. The three configured values are cloned, and
    /// cloning a borrowed `Cow` copies nothing — a default configuration reaches
    /// the wire without a single copy.
    ///
    /// The origin arrives owned and is moved into the header. Taking it by
    /// reference copied it a second time, on top of the copy the caller already
    /// made to carry it into the response future.
    fn apply_cors_headers(
        &self,
        resp: Response,
        origin: Cow<'static, str>,
        preflight: bool,
    ) -> Response {
        let vary = match preflight {
            true => "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
            false => "Origin",
        };
        let resp = resp
            .with_static_header("Access-Control-Allow-Origin", origin)
            .with_static_header("Access-Control-Allow-Methods", self.methods.clone())
            .with_static_header("Access-Control-Allow-Headers", self.headers.clone())
            .with_static_header("Access-Control-Max-Age", self.max_age.clone())
            .with_static_header("Vary", Cow::Borrowed(vary));
        match self.allow_credentials {
            true => {
                resp.with_static_header("Access-Control-Allow-Credentials", Cow::Borrowed("true"))
            }
            false => resp,
        }
    }
}

fn is_preflight(req: &Request) -> bool {
    matches!(req.method_enum(), Some(Method::Options))
        && req.header("access-control-request-method").is_some()
}

fn cors_middleware(config: Arc<CorsBuilder>, req: &Request, next: Next) -> ResponseFuture {
    let origin_ref = match req.header("origin") {
        Some(o) => o,
        None => return next.call(req),
    };

    // `resolve_origin` decides whether the value it answers with is a constant
    // or a copy of the request header, so the copy is made there rather than
    // forced on every answer here. Either way what comes back is owned for
    // `'static`, which is what lets it outlive the borrow of the request.
    let header_origin = match config.resolve_origin(origin_ref) {
        Some(resolved) => resolved,
        None => return next.call(req),
    };

    let preflight = is_preflight(req);
    match preflight {
        true => {
            let resp = config.apply_cors_headers(Response::empty_raw(204), header_origin, true);
            Box::pin(async move { resp })
        }
        false => {
            let resp_fut = next.call(req);
            Box::pin(async move {
                let resp = resp_fut.await;
                config.apply_cors_headers(resp, header_origin, false)
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
) -> impl Fn(&Request, Next) -> ResponseFuture + Send + Sync + 'static {
    builder().origins(origins).build()
}

/// Create a CORS builder for customizing allowed origins, methods, headers, and max-age.
pub fn builder() -> CorsBuilder {
    CorsBuilder {
        origins: Box::default(),
        methods: Cow::Borrowed(DEFAULT_METHODS),
        headers: Cow::Borrowed(DEFAULT_HEADERS),
        max_age: Cow::Borrowed(DEFAULT_MAX_AGE),
        allow_credentials: false,
    }
}
