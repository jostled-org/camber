use super::middleware::Next;
use super::request::Request;
use super::response::Response;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::pin::Pin;

/// Validate the request body as JSON of type `T`.
///
/// Requests with an empty body pass through without validation (e.g. GET).
/// Invalid JSON returns 400 Bad Request with the parse error message.
///
/// The body string is parsed twice: once here for validation and once in the
/// handler via `req.json()`. Both calls deserialize the same cached `&str`
/// (body text is stored in a `OnceLock`). The cost is CPU-only -- no IO or
/// allocation for the body itself. A type-erased cache would eliminate the
/// second parse but adds `Any + Send + Sync` complexity for marginal gain.
pub fn json<T: DeserializeOwned + 'static>()
-> impl Fn(&Request, Next) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync + 'static
{
    move |req, next| {
        if req.body().is_empty() {
            return next.call(req);
        }
        match req.json::<T>() {
            Ok(_) => next.call(req),
            Err(e) => {
                let resp = Response::text_raw(400, &e.to_string());
                Box::pin(async move { resp })
            }
        }
    }
}
