//! The outbound HTTP client.

mod body;
mod buffered;
mod completion;
mod delay;
mod description;
mod dns_edge;
mod egress;
mod error;
mod exchange;
mod handoff;
mod head;
mod intercept;
mod lifetime;
mod operation;
mod pinned;
mod policy;
mod projection;
mod redirect;
mod replay;
mod request;
mod response;
mod reusable;
mod stream;
mod trace;
mod transport;

pub use body::{
    ClientBody, ClientBodyBounds, ClientBodyDriver, ClientBodyFrame, ClientBodyObservation,
};
pub use completion::client_terminal_outcome;
pub use delay::{client_retry_after_delay, client_retry_backoff};
pub(in crate::http) use description::request_target;
pub use dns_edge::ClientDnsEdge;
pub use egress::{EgressPolicy, client_egress_decision};
pub use error::{
    ClientDecode, ClientEgress, ClientError, ClientInput, ClientRedirect, ClientRedirectPolicy,
    ClientTimeout,
};
pub(in crate::http) use head::{HeaderEntry, text_headers};
pub use projection::client_error_projection;
pub use redirect::{
    CrossOriginRedirects, RedirectPolicy, client_next_redirect, client_redirect_destination,
    client_send_total,
};
pub use replay::{RetryPolicy, client_retryable_failure};
pub use request::{ClientRequest, client_header_sensitivity};
pub use response::ClientResponse;
pub use reusable::{
    Client, client, client_dns_edge, client_transport_is_built, delete, delete_with_body, get,
    head, options, patch, patch_form, patch_json, post, post_form, post_json, put, put_form,
    put_json,
};
pub use stream::ClientStreamResponse;
pub(in crate::http) use stream::StreamMetrics;
