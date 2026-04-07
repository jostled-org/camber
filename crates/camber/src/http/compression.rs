use super::middleware::Next;
use super::request::Request;
use super::response::{HeaderPair, Response};
use bytes::Bytes;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::borrow::Cow;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;

/// Minimum response body size (in bytes) to apply compression.
const MIN_COMPRESS_SIZE: usize = 1024;

/// Create compression middleware that gzips eligible text responses.
///
/// Negotiates encoding via `Accept-Encoding`. Compresses responses with
/// text content types (`text/*`, `application/json`, `application/xml`)
/// that exceed 1KB. Skips binary responses, small responses, and
/// requests that don't accept gzip.
fn accepts_gzip(req: &Request) -> bool {
    match req.header("accept-encoding") {
        Some(val) => val
            .split(',')
            .any(|enc| enc.trim().eq_ignore_ascii_case("gzip")),
        None => false,
    }
}

/// Create middleware that applies gzip compression to eligible responses.
///
/// Compression is enabled only when the request advertises `gzip` support and
/// the response is an unencoded text-like payload larger than 1 KB.
pub fn auto()
-> impl Fn(&Request, Next) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync + 'static
{
    move |req, next| {
        let accepts_gzip = accepts_gzip(req);

        let resp_fut = next.call(req);

        Box::pin(async move {
            let resp = resp_fut.await;
            match accepts_gzip {
                false => resp,
                true => try_compress(resp),
            }
        })
    }
}

/// Single-pass scan of headers returning (is_compressible, is_already_encoded).
fn compression_eligibility(headers: &[HeaderPair]) -> (bool, bool) {
    let mut compressible = false;
    let mut encoded = false;

    for (k, v) in headers {
        match () {
            () if k.eq_ignore_ascii_case("Content-Type") => {
                let ct: &str = v.as_ref();
                compressible = ct.starts_with("text/")
                    || ct.starts_with("application/json")
                    || ct.starts_with("application/xml");
            }
            () if k.eq_ignore_ascii_case("Content-Encoding") => {
                encoded = true;
            }
            () => {}
        }
    }

    (compressible, encoded)
}

fn try_compress(resp: Response) -> Response {
    let (compressible, encoded) = compression_eligibility(resp.headers());

    match (
        compressible,
        encoded,
        resp.body_bytes().len() >= MIN_COMPRESS_SIZE,
    ) {
        (true, false, true) => compress_gzip(resp),
        _ => resp,
    }
}

/// Headers that must not be copied from the original response.
/// Content-Type is preserved from the original but must not be duplicated.
/// Content-Length is invalidated by compression.
/// X-Content-Type-Options is not set by the original response builder.
fn is_gzip_skip_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("Content-Length")
}

fn compress_gzip(resp: Response) -> Response {
    let body = resp.body_bytes();
    let mut encoder = GzEncoder::new(Vec::with_capacity(body.len() / 2), Compression::fast());

    match encoder.write_all(body).and_then(|()| encoder.finish()) {
        Ok(compressed) => {
            let status = resp.status();
            let mut headers: Vec<HeaderPair> = resp
                .headers()
                .iter()
                .filter(|(k, _)| !is_gzip_skip_header(k))
                .cloned()
                .collect();
            headers.push((Cow::Borrowed("Content-Encoding"), Cow::Borrowed("gzip")));
            headers.push((Cow::Borrowed("Vary"), Cow::Borrowed("Accept-Encoding")));
            Response::new(status, Bytes::from(compressed), headers)
        }
        Err(err) => {
            tracing::error!(%err, "gzip compression failed");
            resp
        }
    }
}
