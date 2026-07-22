use super::FixtureError;
use super::process::{CapturedOutput, ChildGuard, ReadinessLine};
use std::net::SocketAddr;
use std::process::{Command, ExitStatus};
use std::time::Duration;

pub struct AddressChild {
    child: ChildGuard,
}

impl AddressChild {
    pub fn spawn(command: &mut Command) -> Result<Self, FixtureError> {
        let child = ChildGuard::spawn_output_ready(
            command,
            ReadinessLine {
                expected: None,
                scan_stderr: false,
            },
        )?;
        Ok(Self { child })
    }

    pub fn spawn_current_test(
        test_name: &str,
        child_environment: &str,
        child_value: &str,
        ignored: bool,
    ) -> Result<Self, FixtureError> {
        let executable = std::env::current_exe()?;
        let mut command = Command::new(executable);
        match ignored {
            true => command.args(["--ignored", "--exact", test_name, "--nocapture"]),
            false => command.args(["--exact", test_name, "--nocapture"]),
        };
        command.env(child_environment, child_value);
        Self::spawn(&mut command)
    }

    pub fn wait_for_address(&mut self, timeout: Duration) -> Result<SocketAddr, FixtureError> {
        let line = self.child.receive_readiness(timeout)?;
        match std::str::from_utf8(&line)
            .ok()
            .and_then(|line| line.parse().ok())
        {
            Some(addr) => Ok(addr),
            None => {
                self.child.terminate()?;
                Err(FixtureError::new(
                    "child readiness line was not a socket address",
                ))
            }
        }
    }

    pub fn terminate(&mut self) -> Result<(), FixtureError> {
        self.child.terminate()
    }
}

pub fn run_command(
    command: &mut Command,
    timeout: Duration,
) -> Result<(ExitStatus, CapturedOutput), FixtureError> {
    let mut child = ChildGuard::spawn(command)?;
    let status = child.wait(timeout)?;
    let output = child.captured_output()?;
    Ok((status, output))
}
