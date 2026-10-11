//! The outbound HTTP client.

mod completion;
mod delay;
mod description;
mod error;
mod exchange;
mod intercept;
mod lifetime;
mod operation;
mod policy;
mod projection;
mod redirect;
mod replay;
mod request;
mod response;
mod reusable;
mod trace;

pub use completion::client_terminal_outcome;
pub use delay::{client_retry_after_delay, client_retry_backoff};
pub(in crate::http) use description::request_target;
pub use error::{ClientDecode, ClientError, ClientInput, ClientRedirect, ClientTimeout};
pub use projection::client_error_projection;
pub use redirect::{
    RedirectPolicy, client_next_redirect, client_redirect_destination, client_send_total,
};
pub use replay::{RetryPolicy, client_retryable_transport};
pub use request::{ClientRequest, client_header_sensitivity};
pub use response::ClientResponse;
pub(in crate::http) use response::{HeaderEntry, text_headers};
pub use reusable::{
    Client, client, client_transport_is_built, delete, delete_with_body, get, head, options, patch,
    patch_form, patch_json, post, post_form, post_json, put, put_form, put_json,
};
