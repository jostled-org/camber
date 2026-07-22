#[path = "../support/error.rs"]
mod error;
#[path = "../support/process.rs"]
pub mod process;

pub use error::FixtureError;
pub use process::run_command;
