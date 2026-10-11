//! One request and the collection of its answer.
//!
//! An exchange is what every attempt repeats: the same client and the same
//! validated description, collected under the same response maximum. A
//! followed redirect changes that description, so the attempts after it repeat
//! the hop. It holds no retry policy, no redirect policy, and no deadline. It
//! reads no trace context: the description already holds the one the
//! operation began under. The operation that runs it owns the attempt timer
//! and the operation deadline, and the Reqwest client owns the connect and
//! quiet-interval timers.

use super::super::boundary::ByteBoundary;
use super::super::checked_collect::collect_checked;
use super::description::Description;
use super::projection::collection_failure;
use super::redirect::Carried;
use super::response::{ClientResponse, wire_headers};
use crate::RuntimeError;
use reqwest::{Method, Url};

/// One request as every attempt of it is sent.
pub(super) struct Exchange<'a> {
    client: &'a reqwest::Client,
    /// The validated request. Each attempt clones its headers and shares its
    /// frozen body, so no attempt parses or copies a payload.
    description: Description,
}

impl<'a> Exchange<'a> {
    pub(super) fn new(client: &'a reqwest::Client, description: Description) -> Self {
        Self {
            client,
            description,
        }
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
            .client
            .request(description.method().clone(), description.url().clone())
            .headers(description.headers().clone());
        if !description.body().is_empty() {
            builder = builder.body(description.body().clone());
        }
        builder.send().await
    }

    /// Read one answer's head, then collect its body under the configured
    /// maximum.
    ///
    /// The head is copied first because the collector consumes the response.
    /// A refused or failed collection returns its typed cause alone: the head
    /// is dropped with it, so no partial answer reaches the caller.
    pub(super) async fn collect(
        &self,
        resp: reqwest::Response,
    ) -> Result<ClientResponse, RuntimeError> {
        let status = resp.status().as_u16();
        let headers = wire_headers(resp.headers());

        // No quiet interval of this collection's own: a client's response idle
        // deadline is configured on its Reqwest client and enforced per read
        // there, so arming a second timer here would be a second owner of one
        // dimension.
        let body_bytes = collect_checked(
            resp,
            ByteBoundary::ClientResponse,
            self.description.response_limit(),
            None,
        )
        .await
        .map_err(|failure| collection_failure(&failure))?;

        Ok(ClientResponse::new(status, headers, body_bytes))
    }
}
