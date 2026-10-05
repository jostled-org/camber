//! Bounded commands against one container engine executable.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use super::FixtureError;
use crate::process::{ChildGuard, ProcessError};

/// The bound on one engine command, from spawn to reap.
pub const ENGINE_COMMAND_BOUND: Duration = Duration::from_secs(60);

/// The most engine output an error or log capture retains.
pub const OUTPUT_EXCERPT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct Engine {
    program: Box<Path>,
}

/// What one finished engine command wrote.
struct Reply {
    status: ExitStatus,
    stdout: Box<str>,
    stderr: Box<str>,
}

impl Engine {
    pub fn at(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into().into_boxed_path(),
        }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Run one engine command to exit and return its standard output.
    pub fn run(&self, args: &[&str]) -> Result<Box<str>, FixtureError> {
        self.answer(args, |reply| reply.stdout)
    }

    /// Run one engine command to exit and return both of its streams, for
    /// captures such as service logs that write to standard error.
    pub fn transcript(&self, args: &[&str]) -> Result<Box<str>, FixtureError> {
        self.answer(args, |reply| reply.combined())
    }

    /// Run one engine command to exit and read `output` from a success.
    fn answer(
        &self,
        args: &[&str],
        output: impl FnOnce(Reply) -> Box<str>,
    ) -> Result<Box<str>, FixtureError> {
        let reply = self.execute(args)?;
        match reply.status.success() {
            true => Ok(output(reply)),
            false => Err(reply.failure(args)),
        }
    }

    fn execute(&self, args: &[&str]) -> Result<Reply, FixtureError> {
        let mut command = Command::new(&*self.program);
        command.args(args).stdin(Stdio::null());
        let mut child = ChildGuard::spawn(command, ENGINE_COMMAND_BOUND)
            .map_err(|error| self.spawn_failure(error))?;
        let status =
            child
                .wait_bounded(ENGINE_COMMAND_BOUND)
                .map_err(|error| FixtureError::Engine {
                    command: command_line(args),
                    status: error.to_string().into_boxed_str(),
                    output: Box::default(),
                })?;
        Ok(Reply {
            status,
            stdout: String::from_utf8_lossy(child.stdout()).into(),
            stderr: String::from_utf8_lossy(child.stderr()).into(),
        })
    }

    fn spawn_failure(&self, error: ProcessError) -> FixtureError {
        let program = self.program.display().to_string().into_boxed_str();
        match error {
            ProcessError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
                FixtureError::EngineUnavailable {
                    program,
                    detail: error.to_string().into_boxed_str(),
                }
            }
            error => FixtureError::Engine {
                command: program,
                status: error.to_string().into_boxed_str(),
                output: Box::default(),
            },
        }
    }
}

impl Reply {
    /// Both streams, standard output first, cut to one excerpt.
    fn combined(&self) -> Box<str> {
        excerpt(&format!("{}{}", self.stdout, self.stderr))
    }

    /// The failure a command that exited unsuccessfully reports.
    fn failure(&self, args: &[&str]) -> FixtureError {
        FixtureError::Engine {
            command: command_line(args),
            status: self.status.to_string().into_boxed_str(),
            output: self.combined(),
        }
    }
}

/// The engine arguments as one line, for a failure report.
fn command_line(args: &[&str]) -> Box<str> {
    args.join(" ").into_boxed_str()
}

/// The last [`OUTPUT_EXCERPT_BYTES`] of `text`, cut on a character boundary.
pub fn excerpt(text: &str) -> Box<str> {
    let start = text.len().saturating_sub(OUTPUT_EXCERPT_BYTES);
    let start = (start..=text.len())
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(text.len());
    text[start..].into()
}
