use crate::error::BenchError;
use crate::load::{self, LoadGenerator};
use crate::report::{self, BenchmarkRun, ConcurrencyResult, FrameworkRun};
use crate::servers::go_server;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

// --- Benchmark definitions ---

struct BenchDef {
    name: &'static str,
    path: &'static str,
    tier: u8,
    needs_upstream: bool,
    go_supported: bool,
    frameworks: &'static [FrameworkDef],
}

struct FrameworkDef {
    label: &'static str,
    binary: BinaryKind,
    bench_arg: &'static str,
}

#[derive(Clone, Copy)]
enum BinaryKind {
    Camber,
    Axum,
}

const BENCHMARKS: &[BenchDef] = &[
    // Tier 1 — synthetic, no upstream
    BenchDef {
        name: "hello_text",
        path: "/",
        tier: 1,
        needs_upstream: false,
        go_supported: true,
        frameworks: &[
            FrameworkDef {
                label: "Camber",
                binary: BinaryKind::Camber,
                bench_arg: "hello_text",
            },
            FrameworkDef {
                label: "Axum",
                binary: BinaryKind::Axum,
                bench_arg: "hello_text",
            },
        ],
    },
    BenchDef {
        name: "hello_json",
        path: "/",
        tier: 1,
        needs_upstream: false,
        go_supported: true,
        frameworks: &[
            FrameworkDef {
                label: "Camber",
                binary: BinaryKind::Camber,
                bench_arg: "hello_json",
            },
            FrameworkDef {
                label: "Axum",
                binary: BinaryKind::Axum,
                bench_arg: "hello_json",
            },
        ],
    },
    BenchDef {
        name: "path_param",
        path: "/users/42",
        tier: 1,
        needs_upstream: false,
        go_supported: true,
        frameworks: &[
            FrameworkDef {
                label: "Camber",
                binary: BinaryKind::Camber,
                bench_arg: "path_param",
            },
            FrameworkDef {
                label: "Axum",
                binary: BinaryKind::Axum,
                bench_arg: "path_param",
            },
        ],
    },
    BenchDef {
        name: "static_file",
        path: "/",
        tier: 1,
        needs_upstream: false,
        go_supported: true,
        frameworks: &[
            FrameworkDef {
                label: "Camber",
                binary: BinaryKind::Camber,
                bench_arg: "static_file",
            },
            FrameworkDef {
                label: "Axum",
                binary: BinaryKind::Axum,
                bench_arg: "static_file",
            },
        ],
    },
    // Tier 2 — realistic, needs upstream
    BenchDef {
        name: "db_query",
        path: "/query",
        tier: 2,
        needs_upstream: true,
        go_supported: true,
        frameworks: &[
            FrameworkDef {
                label: "Camber",
                binary: BinaryKind::Camber,
                bench_arg: "db_query",
            },
            FrameworkDef {
                label: "Axum",
                binary: BinaryKind::Axum,
                bench_arg: "db_query",
            },
        ],
    },
    BenchDef {
        name: "middleware_stack",
        path: "/",
        tier: 2,
        needs_upstream: true,
        go_supported: true,
        frameworks: &[
            FrameworkDef {
                label: "Camber",
                binary: BinaryKind::Camber,
                bench_arg: "middleware_stack",
            },
            FrameworkDef {
                label: "Axum",
                binary: BinaryKind::Axum,
                bench_arg: "middleware_stack",
            },
        ],
    },
    BenchDef {
        name: "proxy_forward",
        path: "/",
        tier: 2,
        needs_upstream: true,
        go_supported: true,
        frameworks: &[
            FrameworkDef {
                label: "Camber",
                binary: BinaryKind::Camber,
                bench_arg: "proxy_forward",
            },
            FrameworkDef {
                label: "Axum",
                binary: BinaryKind::Axum,
                bench_arg: "proxy_forward",
            },
        ],
    },
    // Tier 3 — concurrent fan-out, needs upstream
    BenchDef {
        name: "fan_out",
        path: "/fan-out",
        tier: 3,
        needs_upstream: true,
        go_supported: false,
        frameworks: &[
            FrameworkDef {
                label: "Camber",
                binary: BinaryKind::Camber,
                bench_arg: "fan_out",
            },
            FrameworkDef {
                label: "Axum",
                binary: BinaryKind::Axum,
                bench_arg: "fan_out",
            },
        ],
    },
];

// --- Public API ---

/// Paths to the server binaries.
pub struct Binaries {
    pub camber: Box<str>,
    pub axum: Box<str>,
    pub upstream: Box<str>,
}

impl Binaries {
    fn path(&self, kind: BinaryKind) -> &str {
        match kind {
            BinaryKind::Camber => &self.camber,
            BinaryKind::Axum => &self.axum,
        }
    }
}

/// Configuration for an orchestrated benchmark run.
pub struct RunConfig {
    pub tier: Option<u8>,
    pub bench: Option<Box<str>>,
    pub concurrency: Box<[u32]>,
    pub duration: Duration,
}

/// Run benchmarks matching the config, returning results for all matching benchmarks.
/// Progress lines are written to `progress` as each benchmark starts and completes.
pub fn run(
    config: &RunConfig,
    binaries: &Binaries,
    generator: LoadGenerator,
    progress: &mut dyn Write,
) -> Result<Box<[BenchmarkRun]>, BenchError> {
    validate_config(config)?;

    let specs: Box<[&BenchDef]> = BENCHMARKS
        .iter()
        .filter(|b| matches_filter(b, config))
        .collect();

    let go_binary = match go_server::go_available() && specs.iter().any(|bench| bench.go_supported)
    {
        true => Some(go_server::prepare_binary()?),
        false => None,
    };

    let needs_upstream = specs.iter().any(|b| b.needs_upstream);
    let upstream = match needs_upstream {
        true => Some(start_upstream(&binaries.upstream)?),
        false => None,
    };
    let upstream_addr = upstream
        .as_ref()
        .map(|u| SocketAddr::from(([127, 0, 0, 1], u.port)));

    let mut runs = Vec::with_capacity(specs.len());
    for spec in &specs {
        let result = run_one_benchmark(
            spec,
            binaries,
            generator,
            upstream_addr,
            go_binary.as_deref(),
            config,
            progress,
        )?;
        runs.push(result);
    }

    Ok(runs.into_boxed_slice())
}

// --- Internals ---

fn matches_filter(bench: &BenchDef, config: &RunConfig) -> bool {
    match (&config.tier, &config.bench) {
        (Some(t), _) => bench.tier == *t,
        (_, Some(name)) => bench.name == &**name,
        (None, None) => true,
    }
}

fn validate_config(config: &RunConfig) -> Result<(), BenchError> {
    match (&config.tier, &config.bench) {
        (Some(_), Some(_)) => Err(BenchError::InvalidConfig(
            "--tier and --bench cannot be used together".into(),
        )),
        _ => Ok(()),
    }
}

fn run_one_benchmark(
    spec: &BenchDef,
    binaries: &Binaries,
    generator: LoadGenerator,
    upstream_addr: Option<SocketAddr>,
    go_binary: Option<&std::path::Path>,
    config: &RunConfig,
    progress: &mut dyn Write,
) -> Result<BenchmarkRun, BenchError> {
    let go_run = spec.go_supported && go_binary.is_some();
    let capacity = spec.frameworks.len() + usize::from(go_run);
    let mut framework_runs = Vec::with_capacity(capacity);

    for fw in spec.frameworks {
        let _ = writeln!(progress, "[bench] {} / {} ...", spec.name, fw.label);
        let binary = binaries.path(fw.binary);
        let server = start_server(binary, fw.bench_arg, upstream_addr)?;
        let fw_run = bench_server(
            generator,
            server.port,
            spec.path,
            &config.concurrency,
            config.duration,
        )?;
        framework_runs.push(FrameworkRun {
            framework: fw.label.into(),
            results: fw_run,
        });
        drop(server);
        std::thread::sleep(Duration::from_secs(2));
    }

    if go_run {
        let _ = writeln!(progress, "[bench] {} / Go ...", spec.name);
        let upstream_addrs: Box<[SocketAddr]> = go_upstream_addrs(upstream_addr);
        let binary = go_binary.ok_or_else(|| {
            BenchError::ServerStart("go benchmark binary was not prepared".into())
        })?;
        let (addr, _handle) = go_server::start(binary, spec.name, &upstream_addrs)?;
        let fw_run = bench_server(
            generator,
            addr.port(),
            spec.path,
            &config.concurrency,
            config.duration,
        )?;
        framework_runs.push(FrameworkRun {
            framework: "Go".into(),
            results: fw_run,
        });
        std::thread::sleep(Duration::from_secs(2));
    }

    let result = BenchmarkRun {
        name: spec.name.into(),
        frameworks: framework_runs.into_boxed_slice(),
    };

    let table = report::format_one_benchmark(&result);
    let _ = writeln!(progress, "{table}");

    Ok(result)
}

fn bench_server(
    generator: LoadGenerator,
    port: u16,
    path: &str,
    concurrency: &[u32],
    duration: Duration,
) -> Result<Box<[ConcurrencyResult]>, BenchError> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let raw = load::three_phase_bench(generator, &url, concurrency, duration)?;
    Ok(raw
        .into_vec()
        .into_iter()
        .map(|(concurrency, result)| ConcurrencyResult {
            concurrency,
            result,
        })
        .collect())
}

/// Build the upstream address slice for the Go server.
fn go_upstream_addrs(upstream: Option<SocketAddr>) -> Box<[SocketAddr]> {
    match upstream {
        Some(addr) => Box::new([addr]),
        None => Box::new([]),
    }
}

// --- Subprocess management ---

struct ServerProcess {
    child: Child,
    port: u16,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find_free_port() -> Result<u16, BenchError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn start_server(
    binary: &str,
    bench: &str,
    upstream: Option<SocketAddr>,
) -> Result<ServerProcess, BenchError> {
    let port = find_free_port()?;
    let port_str = port.to_string();
    let upstream_str = upstream.map(|a| a.to_string());

    let mut args = vec!["--bench", bench, "--port", &port_str];
    if let Some(ref addr) = upstream_str {
        args.push("--upstream");
        args.push(addr);
    }

    let mut child = Command::new(binary)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            BenchError::ServerStart(format!("failed to spawn {binary}: {e}").into_boxed_str())
        })?;

    wait_for_ready(&mut child, binary)?;
    Ok(ServerProcess { child, port })
}

fn start_upstream(binary: &str) -> Result<ServerProcess, BenchError> {
    let port = find_free_port()?;
    let port_str = port.to_string();

    let mut child = Command::new(binary)
        .args(["--port", &port_str])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            BenchError::ServerStart(format!("failed to spawn upstream: {e}").into_boxed_str())
        })?;

    wait_for_ready(&mut child, binary)?;
    Ok(ServerProcess { child, port })
}

fn wait_for_ready(child: &mut Child, binary: &str) -> Result<(), BenchError> {
    let stdout = child.stdout.take().ok_or_else(|| {
        BenchError::ServerStart(format!("no stdout from {binary}").into_boxed_str())
    })?;

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| {
        BenchError::ServerStart(format!("reading stdout from {binary}: {e}").into_boxed_str())
    })?;

    match line.trim() == "ready" {
        true => Ok(()),
        false => {
            let stderr_output = read_stderr(child);
            Err(BenchError::ServerStart(
                format!("{binary} did not print 'ready', got: {line:?}\nstderr: {stderr_output}")
                    .into_boxed_str(),
            ))
        }
    }
}

fn read_stderr(child: &mut Child) -> String {
    let Some(stderr) = child.stderr.take() else {
        return "<stderr unavailable>".to_owned();
    };
    let mut reader = BufReader::new(stderr);
    let mut buf = String::new();
    match reader.read_line(&mut buf) {
        Ok(_) => buf,
        Err(e) => format!("<stderr read failed: {e}>"),
    }
}

/// Resolve binary paths from the same directory as the current executable.
/// Used by the orchestrator binary (main.rs).
pub fn binaries_from_current_exe() -> Result<Binaries, BenchError> {
    // Build all bench binaries before looking for them.
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "camber-bench"])
        .status()
        .map_err(|e| {
            BenchError::ServerStart(format!("failed to run cargo build: {e}").into_boxed_str())
        })?;
    match status.success() {
        true => {}
        false => {
            return Err(BenchError::ServerStart(
                "cargo build --release -p camber-bench failed".into(),
            ));
        }
    }

    let exe = std::env::current_exe().map_err(|e| {
        BenchError::ServerStart(format!("cannot find current exe: {e}").into_boxed_str())
    })?;
    let dir = exe
        .parent()
        .ok_or_else(|| BenchError::ServerStart("current exe has no parent directory".into()))?;

    let resolve = |name: &str| -> Result<Box<str>, BenchError> {
        let path = dir.join(name);
        match path.exists() {
            true => Ok(path.to_string_lossy().into_owned().into_boxed_str()),
            false => Err(BenchError::ServerStart(
                format!("binary not found: {}", path.display()).into_boxed_str(),
            )),
        }
    };

    Ok(Binaries {
        camber: resolve("bench-camber")?,
        axum: resolve("bench-axum")?,
        upstream: resolve("bench-upstream")?,
    })
}
