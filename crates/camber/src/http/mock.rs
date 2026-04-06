use super::Method;
use super::Response;
use super::response::HeaderPair;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Global registry of mock HTTP responses.
///
/// When a mock is registered, `http::get`/`http::post` check this registry
/// before making a real network call. Mocks are keyed by (method, URL).
/// Uses a Vec for linear scan — the registry is test-only with few entries.
static MOCK_ACTIVE: AtomicBool = AtomicBool::new(false);
static MOCK_REGISTRY: Mutex<Option<Vec<MockEntry>>> = Mutex::new(None);

struct MockEntry {
    method: Option<Method>,
    url: Box<str>,
    status: u16,
    body: bytes::Bytes,
    headers: Arc<[HeaderPair]>,
    call_count: Arc<AtomicUsize>,
}

fn with_registry<F, R>(f: F) -> R
where
    F: FnOnce(&mut Vec<MockEntry>) -> R,
{
    let mut guard = MOCK_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let entries = guard.get_or_insert_with(Vec::new);
    f(entries)
}

/// Check the mock registry for a matching (method, URL) pair.
/// Returns Some(Response) if a mock is registered, None otherwise.
///
/// Matching priority: exact method match first, then method-agnostic (None).
pub(crate) fn try_intercept(method: Method, url: &str) -> Option<Response> {
    if !MOCK_ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    with_registry(|entries| {
        let entry = find_mock_entry(entries, method, url)?;
        entry.call_count.fetch_add(1, Ordering::Release);
        let headers: Vec<HeaderPair> = entry
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Some(Response::new(entry.status, entry.body.clone(), headers))
    })
}

fn find_mock_entry<'a>(
    entries: &'a [MockEntry],
    method: Method,
    url: &str,
) -> Option<&'a MockEntry> {
    entries
        .iter()
        .find(|e| e.url.as_ref() == url && e.method == Some(method))
        .or_else(|| {
            entries
                .iter()
                .find(|e| e.url.as_ref() == url && e.method.is_none())
        })
}

/// Register a method-agnostic mock for an outbound HTTP URL.
///
/// Matches any HTTP method. Use `http_method` for method-specific mocks.
/// Returns a `MockHttpBuilder` to configure the canned response.
pub fn http(url: &str) -> MockHttpBuilder {
    MockHttpBuilder {
        method: None,
        url: url.into(),
        response: None,
    }
}

/// Register a method-specific mock for an outbound HTTP URL.
///
/// Only matches requests with the given HTTP method.
/// Returns a `MockHttpBuilder` to configure the canned response.
pub fn http_method(method: Method, url: &str) -> MockHttpBuilder {
    MockHttpBuilder {
        method: Some(method),
        url: url.into(),
        response: None,
    }
}

/// Builder for configuring a mock HTTP response.
pub struct MockHttpBuilder {
    method: Option<Method>,
    url: Box<str>,
    response: Option<Response>,
}

impl MockHttpBuilder {
    /// Set the canned response to return when the URL is requested.
    pub fn returns(mut self, response: Response) -> MockHttp {
        self.response = Some(response);
        self.install()
    }

    fn install(self) -> MockHttp {
        let resp = match self.response {
            Some(r) => r,
            None => Response::empty_raw(200),
        };
        let call_count = Arc::new(AtomicUsize::new(0));
        let method = self.method;
        let url = self.url.clone();
        let entry = MockEntry {
            method,
            url: self.url,
            status: resp.status(),
            body: bytes::Bytes::copy_from_slice(resp.body_bytes()),
            headers: resp.headers().to_vec().into(),
            call_count: Arc::clone(&call_count),
        };
        with_registry(|entries| {
            entries.push(entry);
            MOCK_ACTIVE.store(true, Ordering::Release);
        });
        MockHttp {
            method,
            url,
            call_count,
        }
    }
}

/// Handle to a registered mock. Use to assert call counts.
///
/// The mock is automatically deregistered when this handle is dropped.
pub struct MockHttp {
    method: Option<Method>,
    url: Box<str>,
    call_count: Arc<AtomicUsize>,
}

impl MockHttp {
    /// Panics if the mock was not called exactly once.
    pub fn assert_called_once(&self) {
        let count = self.call_count.load(Ordering::Acquire);
        assert!(
            count == 1,
            "expected mock for {} {} to be called once, was called {count} times",
            match self.method {
                Some(m) => m.as_str(),
                None => "*",
            },
            self.url
        );
    }
}

impl Drop for MockHttp {
    fn drop(&mut self) {
        let method = self.method;
        let url = &self.url;
        with_registry(|entries| {
            entries.retain(|e| !(e.url == *url && e.method == method));
            if entries.is_empty() {
                MOCK_ACTIVE.store(false, Ordering::Release);
            }
        });
    }
}
