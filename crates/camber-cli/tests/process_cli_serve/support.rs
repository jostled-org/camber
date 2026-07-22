#[path = "../support/error.rs"]
mod error;
#[path = "../support/http.rs"]
pub mod http;
#[path = "../support/process.rs"]
pub mod process;

pub use error::FixtureError;

impl From<http::BackendError> for FixtureError {
    fn from(error: http::BackendError) -> Self {
        Self::new(error.to_string())
    }
}
