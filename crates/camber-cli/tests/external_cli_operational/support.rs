#[path = "../support/error.rs"]
mod error;
#[path = "../support/generated_project.rs"]
pub mod generated_project;
#[path = "../support/process.rs"]
pub mod process;

pub use error::FixtureError;
pub use process::run_command;
