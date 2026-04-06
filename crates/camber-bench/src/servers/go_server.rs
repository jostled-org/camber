use crate::error::BenchError;
use crate::servers::ServerHandle;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::OnceLock;

/// Check whether `go` is on PATH.
pub fn go_available() -> bool {
    Command::new("go")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Start a Go benchmark server for the given benchmark name.
/// `upstream_addrs` is used for proxy_forward and fan_out benchmarks.
pub fn start(
    binary: &Path,
    bench: &str,
    upstream_addrs: &[SocketAddr],
) -> Result<(SocketAddr, ServerHandle), BenchError> {
    let mut args = vec!["--bench".into(), bench.to_owned()];
    if !upstream_addrs.is_empty() {
        let csv: String = upstream_addrs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        args.push("--upstream".into());
        args.push(csv);
    }

    let mut child = Command::new(binary)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            BenchError::ServerStart(format!("failed to spawn go server: {e}").into_boxed_str())
        })?;

    let addr = read_addr_from_child(&mut child)?;

    let thread = std::thread::spawn(move || {
        let _ = child.wait();
    });

    std::thread::sleep(std::time::Duration::from_millis(50));
    Ok((addr, ServerHandle::new(thread)))
}

pub fn prepare_binary() -> Result<PathBuf, BenchError> {
    static GO_BINARY: OnceLock<Result<PathBuf, Box<str>>> = OnceLock::new();

    match GO_BINARY.get_or_init(build_go_binary) {
        Ok(binary) => Ok(binary.clone()),
        Err(message) => Err(BenchError::ServerStart(message.clone())),
    }
}

fn go_source_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("go")
}

fn go_binary_path() -> PathBuf {
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
    target_dir.join("go-bench")
}

fn build_go_binary() -> Result<PathBuf, Box<str>> {
    let source_dir = go_source_dir();
    let binary = go_binary_path();

    let output = Command::new("go")
        .args(["build", "-o"])
        .arg(&binary)
        .arg("main.go")
        .current_dir(&source_dir)
        .output()
        .map_err(|e| format!("go build failed: {e}").into_boxed_str())?;

    match output.status.success() {
        true => Ok(binary),
        false => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("go build failed: {stderr}").into_boxed_str())
        }
    }
}

fn read_addr_from_child(child: &mut Child) -> Result<SocketAddr, BenchError> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| BenchError::ServerStart("no stdout from go process".into()))?;

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| {
        BenchError::ServerStart(format!("reading go server addr: {e}").into_boxed_str())
    })?;

    let trimmed = line.trim();
    trimmed.parse().map_err(|e| {
        BenchError::ServerStart(format!("parsing go server addr '{trimmed}': {e}").into_boxed_str())
    })
}
