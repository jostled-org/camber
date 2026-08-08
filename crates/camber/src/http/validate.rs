use super::middleware::{MiddlewareFuture, Next};
use super::request::Request;
use crate::RuntimeError;
use serde::de::DeserializeOwned;

/// Validate the request body as JSON of type `T`.
///
/// Requests with an empty body pass through without validation (e.g. GET).
/// Invalid JSON becomes a malformed-body rejection, answered by the router's
/// configured rejection mapper or by the built-in `400`.
///
/// A validation failure leaves as the parser's own `RuntimeError`, not as a
/// response: the router's rejection boundary classifies it as a malformed body
/// and decides what the peer is told, so this frame never rebuilds that
/// decision from the parser's text.
///
/// The body is parsed twice: once here for validation and once in the handler
/// via `req.json()`. Both parse the same `Bytes` buffer, so the cost is a
/// second deserialize with no allocation and no second decode. A type-erased
/// cache would eliminate the second parse but adds `Any + Send + Sync`
/// complexity for marginal gain.
pub fn json<T: DeserializeOwned + 'static>()
-> impl Fn(&Request, Next) -> MiddlewareFuture + Send + Sync + 'static {
    move |req, next| match validated::<T>(req) {
        Ok(()) => {
            let passed = next.call(req);
            Box::pin(async move { Ok(passed.await) })
        }
        Err(refused) => Box::pin(async move { Err(refused) }),
    }
}

/// Whether this body is the representation the route declared.
///
/// An empty body carries no representation to check, so it passes: a `GET`
/// through this frame is not a malformed `POST`.
///
/// Emptiness is read off the raw bytes. Asking the text view instead decoded
/// the whole body to answer a length question, and cached a lossy copy of every
/// body that is not UTF-8.
fn validated<T: DeserializeOwned>(req: &Request) -> Result<(), RuntimeError> {
    match req.body_bytes().is_empty() {
        true => Ok(()),
        false => req.json::<T>().map(|_parsed| ()),
    }
}
