//! The body of one outbound stream: its owner, and the caller's one handle.
//!
//! The owner reads the source. The handle places one demand at a time and
//! takes the one chunk the owner admitted for it. Both write one shared
//! state, and that state holds the one terminal fact of the body.

mod driver;
mod handle;
mod owner;
mod state;

pub(super) use driver::SourceCounts;
pub use driver::{ClientBodyBounds, ClientBodyDriver, ClientBodyFrame, ClientBodyObservation};
pub(super) use handle::BareOwner;
pub use handle::ClientBody;
pub(super) use owner::{BodyAccess, BodyBudget, BodyOwner, BodyWatch};
