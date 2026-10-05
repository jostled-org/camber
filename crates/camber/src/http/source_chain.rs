//! The one rendering of an error and its causes.

use std::error::Error;
use std::fmt;

/// One error and everything it was caused by, written as a single line.
///
/// A borrowed view rather than a built string: the chain is walked while the
/// subscriber writes it, so a refusal nobody is recording pays nothing to
/// format one.
///
/// The walk ends at an integration error: what it was caused by is third-party
/// text that can carry credentials, payloads, and URLs.
///
/// Visible to the rest of the HTTP module because it is the one spelling of
/// this walk. A second copy renders `error: cause: cause` the same way until
/// one of them is corrected, and then two callers disagree about what a cause
/// chain reads as. A caller that needs the text owned calls `to_string` on it;
/// the walk itself is still written once.
pub(super) struct SourceChain<'a>(pub(super) &'a (dyn Error + 'static));

impl fmt::Display for SourceChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut rendered = self.0;
        while let Some(cause) = next_rendered_cause(rendered) {
            write!(f, ": {cause}")?;
            rendered = cause;
        }
        Ok(())
    }
}

/// The cause a chain renders after `error`, or `None` where the chain ends.
fn next_rendered_cause<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a (dyn Error + 'static)> {
    match crate::error::hides_sources(error) {
        true => None,
        false => error.source(),
    }
}
