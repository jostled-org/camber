//! Multipart request parsing, in two ownership models over one grammar.
//!
//! [`buffered`] materializes a body that is already in memory into finished
//! [`Part`] values. [`session`] reads a body that is still arriving, one bounded
//! field at a time, under limits the route configured. They share [`grammar`],
//! so a body one of them accepts is a body the other accepts.
//!
//! This file declares and re-exports. It holds no parsing, no accounting, and no
//! ownership of its own.

mod budget;
mod buffered;
mod grammar;
mod limits;
mod parser;
mod session;

pub use buffered::{MultipartReader, Part};
pub use limits::{MultipartLimits, MultipartLimitsBuilder};
pub use session::{MultipartField, MultipartStream};

pub(in crate::http) use buffered::parse;
pub(in crate::http) use grammar::request_boundary;
pub(in crate::http) use session::{
    MultipartCompletion, MultipartFailure, MultipartRevocation, MultipartSessionDriver,
    MultipartTerminal, SessionMetrics, SessionObserver, open,
};
