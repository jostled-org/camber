//! The mock answer for one described call, when one is registered.

use super::super::method::Method as LocalMethod;
use super::super::mock;
use super::description::Description;
use super::error::ClientError;
use super::response::ClientResponse;
use crate::RuntimeError;

/// The mock answer registered for this description, or `None` when the
/// network answers.
///
/// An extension method is not one of the routed methods, so it has no
/// method-specific registration to match and only a method-agnostic one
/// answers it.
pub(super) fn mock_answer(description: &Description) -> Option<ClientResponse> {
    mock::try_intercept(
        LocalMethod::from_reqwest(description.method()),
        description.url().as_str(),
    )
}

/// Return a mock answer under the ceiling a real answer is collected under.
///
/// A mock body is already whole, so its length is the only fact to check. One
/// above the ceiling is refused with the collector's own typed cause.
pub(super) fn admit_mock(
    answer: ClientResponse,
    response_limit: Option<usize>,
) -> Result<ClientResponse, RuntimeError> {
    match response_limit {
        Some(limit) if answer.body_bytes().len() > limit => {
            Err(RuntimeError::Client(ClientError::ResponseTooLarge {
                limit,
            }))
        }
        _ => Ok(answer),
    }
}
