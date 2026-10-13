//! The mock answer for one described call, when one is registered.

use super::super::method::Method as LocalMethod;
use super::super::mock;
use super::completion::Completion;
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

/// Record a mock answer's status, then return the answer under the ceiling a
/// real answer is collected under.
///
/// The status is recorded first, so a refused mock still records the status
/// it was refused with. Both terminals admit a mock here, so that order
/// exists once.
pub(super) fn admit_recorded(
    answer: ClientResponse,
    response_limit: Option<usize>,
    completion: &mut Completion,
) -> Result<ClientResponse, RuntimeError> {
    completion.final_head(answer.status());
    admit_mock(answer, response_limit)
}

/// Return a mock answer under the ceiling a real answer is collected under.
///
/// A mock body is already whole, so its length is the only fact to check. One
/// above the ceiling is refused with the collector's own typed cause.
fn admit_mock(
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
