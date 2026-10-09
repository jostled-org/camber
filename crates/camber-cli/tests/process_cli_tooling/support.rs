#[path = "../support/error.rs"]
mod error;
#[path = "../support/generated_project.rs"]
pub mod generated_project;
#[path = "../support/http_head.rs"]
pub mod http_head;
#[path = "../support/process.rs"]
pub mod process;

pub use error::FixtureError;
pub use process::{
    CONFIG_REFUSAL_BOUND, failed_checks, is_fifo, make_fifo, run_command, run_command_with_timeout,
    run_command_within, serve_command,
};
