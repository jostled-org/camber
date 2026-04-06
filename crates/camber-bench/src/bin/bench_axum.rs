use std::io::Write;
use std::net::SocketAddr;

#[derive(clap::Parser)]
#[command(name = "bench-axum", about = "Standalone axum benchmark server")]
struct Cli {
    /// Benchmark name
    #[arg(long)]
    bench: String,

    /// Port to listen on
    #[arg(long)]
    port: u16,

    /// Upstream mock address (required for tier 2/3 benchmarks)
    #[arg(long)]
    upstream: Option<SocketAddr>,
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();

    let app = match camber_bench::servers::axum_server::build_app(&cli.bench, cli.upstream) {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "error: {e}");
            std::process::exit(1);
        }
    };

    let listener = match std::net::TcpListener::bind(("127.0.0.1", cli.port)) {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "listen error: {e}");
            std::process::exit(1);
        }
    };
    match listener.set_nonblocking(true) {
        Ok(()) => {}
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "listen error: {e}");
            std::process::exit(1);
        }
    }

    println!("ready");
    let thread = camber_bench::servers::axum_server::spawn_axum_runtime(listener, app);
    let _ = thread.join();
}
