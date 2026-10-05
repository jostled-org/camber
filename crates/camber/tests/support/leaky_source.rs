//! A third-party failure that leaks what it failed on.
//!
//! The shape an SDK error takes when it echoes the request it failed: both
//! its `Display` and its `Debug` carry the text it was built with. Camber
//! keeps such a value inspectable through `source`, and must not print it.

use std::error::Error;
use std::fmt;

/// A bearer or API token.
pub const TOKEN: &str = "tok-sentinel-5f1c";

/// An access key and its secret.
pub const CREDENTIALS: &str = "AKIASENTINELCRED/sk-sentinel-93ab";

/// A message body.
pub const PAYLOAD: &str = "payload-sentinel-order-4471";

/// An SQS receipt handle.
pub const RECEIPT_HANDLE: &str = "receipt-sentinel-AQEBx9";

/// The userinfo half of a URL.
pub const USERINFO: &str = "userinfo-sentinel:pw-sentinel";

/// The query half of a URL.
pub const QUERY: &str = "X-Amz-Security-Token=query-sentinel-77";

/// Every secret [`LeakySource::every_secret`] carries; a diagnostic must
/// repeat none of them.
pub const SECRETS: [&str; 6] = [TOKEN, CREDENTIALS, PAYLOAD, RECEIPT_HANDLE, USERINFO, QUERY];

/// A third-party failure whose own `Display` and `Debug` carry its text.
pub struct LeakySource(Box<str>);

impl LeakySource {
    /// A failure whose renderings echo `text`.
    pub fn echoing(text: impl Into<Box<str>>) -> Self {
        Self(text.into())
    }

    /// A failure that echoes every one of [`SECRETS`]: the URL with its
    /// userinfo and query, the credential it signed with, and the message it
    /// was sending.
    pub fn every_secret() -> Self {
        Self::echoing(format!(
            "request to https://{USERINFO}@queue.local.test/000000000000/orders?{QUERY} \
             failed: authorization=Bearer {TOKEN} credentials={CREDENTIALS} \
             body={PAYLOAD} receipt={RECEIPT_HANDLE}"
        ))
    }
}

impl fmt::Display for LeakySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for LeakySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LeakySource({})", self.0)
    }
}

impl Error for LeakySource {}
