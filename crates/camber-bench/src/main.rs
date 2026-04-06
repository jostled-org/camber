use camber_bench::{load, loc, orchestrate, report};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

#[derive(clap::Parser)]
#[command(
    name = "camber-bench",
    about = "Benchmark suite for the Camber framework"
)]
struct Cli {
    /// Benchmark tier to run (1=synthetic, 2=realistic, 3=escape-hatch)
    #[arg(long)]
    tier: Option<u8>,

    /// Run a single benchmark by name
    #[arg(long)]
    bench: Option<String>,

    /// Output results as JSON
    #[arg(long)]
    json: bool,

    /// Seconds per measured run (default 15)
    #[arg(long, default_value = "15")]
    duration: u64,

    /// Comma-separated concurrency levels (default 16,64,256)
    #[arg(long, value_delimiter = ',', default_values_t = vec![16, 64, 256])]
    concurrency: Vec<u32>,
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();

    let generator = match load::detect_load_generator() {
        Some(g) => g,
        None => {
            let _ = writeln!(
                std::io::stderr(),
                "error: no load generator found. Install wrk or oha:\n  \
                 brew install wrk    # macOS\n  \
                 cargo install oha   # cross-platform"
            );
            std::process::exit(1);
        }
    };

    let binaries = match orchestrate::binaries_from_current_exe() {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "error: {e}");
            std::process::exit(1);
        }
    };

    let config = orchestrate::RunConfig {
        tier: cli.tier,
        bench: cli.bench.map(|s| s.into_boxed_str()),
        concurrency: cli.concurrency.into_boxed_slice(),
        duration: Duration::from_secs(cli.duration),
    };

    let runs = match orchestrate::run(&config, &binaries, generator, &mut std::io::stderr()) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "benchmark error: {e}");
            std::process::exit(1);
        }
    };

    let output = format_output(cli.json, &runs);
    print!("{output}");
}

fn format_output(json: bool, runs: &[report::BenchmarkRun]) -> Box<str> {
    match json {
        true => report::format_json(runs).unwrap_or_else(|e| {
            let _ = writeln!(std::io::stderr(), "json error: {e}");
            std::process::exit(1);
        }),
        false => compute_loc().map_or_else(
            |e| {
                let _ = writeln!(std::io::stderr(), "loc error: {e}");
                std::process::exit(1);
            },
            |loc_comparison| report::format_markdown_with_loc(runs, &loc_comparison),
        ),
    }
}

fn compute_loc() -> std::io::Result<report::LocComparison> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let servers = manifest.join("src/servers");
    let camber = loc::count_source_loc(&servers.join("camber_server.rs"))?;
    let axum = loc::count_source_loc(&servers.join("axum_server.rs"))?;
    let go = loc::count_source_loc(&manifest.join("go/main.go"))?;

    Ok(report::LocComparison {
        camber_loc: camber,
        axum_loc: axum,
        go_loc: go,
    })
}
