//! A guarded spawn retries a program that another process still holds open
//! for writing, and nothing else.
//!
//! A sibling thread that forks while a fixture writes a script hands its child
//! the write end until that child execs. Inside that window, Linux refuses to
//! run the script with `ExecutableFileBusy`. The window closes on its own, so
//! the spawn is retried a bounded number of times.

use crate::process::retry_busy;
use std::io;
use std::time::Duration;

fn busy() -> io::Error {
    io::Error::from(io::ErrorKind::ExecutableFileBusy)
}

#[test]
fn a_busy_program_is_retried_until_it_starts() {
    let mut calls = 0;
    let started = retry_busy(3, Duration::ZERO, || {
        calls += 1;
        match calls {
            1 | 2 => Err(busy()),
            _ => Ok(calls),
        }
    });
    assert_eq!(started.unwrap(), 3);
}

#[test]
fn a_program_that_stays_busy_fails_at_the_bound() {
    let mut calls = 0;
    let refused = retry_busy(3, Duration::ZERO, || -> io::Result<()> {
        calls += 1;
        Err(busy())
    });
    assert_eq!(
        refused.map_err(|error| error.kind()),
        Err(io::ErrorKind::ExecutableFileBusy)
    );
    assert_eq!(calls, 3, "attempts made");
}

#[test]
fn any_other_failure_is_returned_at_once() {
    let mut calls = 0;
    let refused = retry_busy(3, Duration::ZERO, || -> io::Result<()> {
        calls += 1;
        Err(io::Error::from(io::ErrorKind::NotFound))
    });
    assert_eq!(
        refused.map_err(|error| error.kind()),
        Err(io::ErrorKind::NotFound)
    );
    assert_eq!(calls, 1, "attempts made");
}
