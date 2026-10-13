//! One request as every attempt of it is sent.
//!
//! An exchange is what every attempt repeats: the same transport and the same
//! validated description. A followed redirect changes that description, so the
//! attempts after it repeat the hop. It holds no retry policy, no redirect
//! policy, and no deadline. It reads no trace context: the description already
//! holds the one the operation began under. The operation that runs it owns
//! the attempt timer and the operation deadline, and the Reqwest client owns
//! the connect and quiet-interval timers.
//!
//! An exchange sends and does not collect. It ends at the final head, where it
//! gives up its transport and drops its request.

use super::description::Description;
use super::redirect::Carried;
use super::transport::OperationTransport;
use reqwest::{Method, Url};

/// One request as every attempt of it is sent.
pub(super) struct Exchange<'t> {
    /// The transport every attempt and every hop of this operation sends
    /// through. The exchange owns it until the final head takes it.
    transport: OperationTransport<'t>,
    /// The validated request. Each attempt clones its headers and shares its
    /// frozen body, so no attempt parses or copies a payload.
    description: Description,
}

impl<'t> Exchange<'t> {
    pub(super) fn new(transport: OperationTransport<'t>, description: Description) -> Self {
        Self {
            transport,
            description,
        }
    }

    /// End the exchange at its final head.
    ///
    /// The transport and the response maximum are what the body is read
    /// under. The request is dropped here, so no attempt follows.
    pub(super) fn finish(self) -> (OperationTransport<'t>, Option<usize>) {
        (self.transport, self.description.response_limit())
    }

    pub(super) fn method(&self) -> &Method {
        self.description.method()
    }

    /// The URL the next attempt is sent to.
    pub(super) fn url(&self) -> &Url {
        self.description.url()
    }

    /// Make the request the one an authorized redirect asks for.
    pub(super) fn follow(&mut self, url: Url, carried: Carried) {
        self.description.retarget(url);
        match carried {
            Carried::Whole => {}
            Carried::Bodiless(method) => self.description.become_bodiless(method),
        }
    }

    /// The host this request is sent to.
    ///
    /// The only part of the target a diagnostic may name: the path and the
    /// query can hold what the caller did not mean to publish.
    pub(super) fn host(&self) -> &str {
        self.description.url().host_str().unwrap_or_default()
    }

    /// Send one attempt and wait for its response head.
    ///
    /// An empty body is no body: the transport then declares no content.
    pub(super) async fn send(&self) -> Result<reqwest::Response, reqwest::Error> {
        let description = &self.description;
        let mut builder = self
            .transport
            .client()
            .request(description.method().clone(), description.url().clone())
            .headers(description.headers().clone());
        if !description.body().is_empty() {
            builder = builder.body(description.body().clone());
        }
        builder.send().await
    }
}
