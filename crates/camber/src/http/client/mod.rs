//! The outbound HTTP client.

mod builder;
mod delay;
mod dispatch;
mod exchange;
mod replay;
mod sequence;

pub use builder::{
    ClientBuilder, client, delete, delete_with_body, get, head, options, patch, patch_form,
    patch_json, post, post_form, post_json, put, put_form, put_json,
};
pub use delay::{client_retry_after_delay, client_retry_backoff};
pub use replay::client_retryable_transport;
