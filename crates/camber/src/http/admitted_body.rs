//! The owned take of a buffered request body.
//!
//! A buffered request holds its body and its admission permit until it ends.
//! [`Request::take_body`](super::Request::take_body) moves both into one owned
//! value, so a handler can return the admission place, and free the bytes,
//! before its response is ready.

use super::body_admission::BodyPermit;
use bytes::Bytes;
use std::fmt;
use std::ops::Deref;

/// A buffered request body, taken from its request together with its
/// admission place.
///
/// Dropping it returns the place to the application's permit pool and drops
/// this reference to the bytes. [`AdmittedBody::into_bytes`] returns the place
/// and keeps the bytes. Read the bytes through `Deref<Target = [u8]>` or
/// `AsRef<[u8]>`.
///
/// It owns the permit that
/// [`BodyAdmission::with_permit`](super::BodyAdmission::with_permit) supplied,
/// when the route's policy supplied one. Without a permit, dropping it only
/// drops the bytes.
///
/// ```
/// use camber::http::{Request, Response, Router};
///
/// let mut router = Router::new();
/// router.post("/jobs", |req: &Request| {
///     let job = req.json::<Vec<u64>>();
///     // Parsed: return the admission place before the slow work.
///     drop(req.take_body());
///     async move {
///         let total: u64 = job?.iter().sum();
///         Response::json(200, &total)
///     }
/// });
/// ```
#[must_use = "bind it to hold the admission place, or drop it to return the place"]
pub struct AdmittedBody {
    bytes: Bytes,
    /// Held only so that dropping this value releases it, once.
    permit: Option<BodyPermit>,
}

impl AdmittedBody {
    /// Return the admission place now, and keep the bytes.
    ///
    /// The bytes then belong to no admission pool. Budget them as you budget
    /// a [`Request::body_shared`](super::Request::body_shared) handle.
    #[must_use = "drop the AdmittedBody instead to return the place without keeping the bytes"]
    pub fn into_bytes(self) -> Bytes {
        let Self { bytes, permit } = self;
        drop(permit);
        bytes
    }
}

impl Deref for AdmittedBody {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl AsRef<[u8]> for AdmittedBody {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for AdmittedBody {
    /// Names the length and the permit. Never prints the payload.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmittedBody")
            .field("len", &self.bytes.len())
            .field("holds_permit", &self.permit.is_some())
            .finish()
    }
}

/// What became of one request's buffered body.
///
/// The take-once cell behind [`Request::take_body`](super::Request::take_body).
/// The permit moves out at most once, so request end after a take has no
/// permit left to release a second time.
pub(super) enum BodyClaim {
    /// The route buffers no body, so there is nothing to take.
    Unbuffered,
    /// The buffered body is not taken. Holds the admission permit, when the
    /// application supplied one.
    Held(Option<BodyPermit>),
    /// A handler took the body and its permit.
    Taken,
}

impl BodyClaim {
    /// Take the buffered body once, with the permit this claim holds.
    pub(super) fn take(&mut self, bytes: &Bytes) -> Option<AdmittedBody> {
        match self {
            Self::Held(permit) => {
                let permit = permit.take();
                *self = Self::Taken;
                Some(AdmittedBody {
                    bytes: bytes.clone(),
                    permit,
                })
            }
            Self::Unbuffered | Self::Taken => None,
        }
    }

    /// Whether a handler took the body.
    pub(super) fn is_taken(&self) -> bool {
        matches!(self, Self::Taken)
    }
}
