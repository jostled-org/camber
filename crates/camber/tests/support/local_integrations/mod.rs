//! Owned local-service fixtures for the M9 integration lanes.
//!
//! A fixture owns one run-scoped network, the containers it starts on that
//! network, and one temporary root for their configuration. Readiness is a
//! protocol acknowledgement from the published loopback port, never process
//! existence. Teardown names every owned resource and proves each is gone;
//! `Drop` is the fallback after an assertion unwinds.
//!
//! Every mounting root provides `delivery_fixture`, `http`, `process`, and
//! `temp_support` at its crate root.

pub mod engine;
pub mod engine_stub;
mod error;
pub mod readiness;
pub mod service;

pub use error::FixtureError;
