use std::io::Write;
use std::net::SocketAddr;

#[derive(clap::Parser)]
#[command(name = "bench-camber", about = "Standalone camber benchmark server")]
struct Cli {
    /// Benchmark name
    #[arg(long)]
    bench: String,

    /// Port to listen on
    #[arg(long)]
    port: u16,

    /// Upstream mock address (required for tier 2+ benchmarks)
    #[arg(long)]
    upstream: Option<SocketAddr>,
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();

    let router = match camber_bench::servers::camber_server::build_router(&cli.bench, cli.upstream)
    {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "error: {e}");
            std::process::exit(1);
        }
    };

    let addr = format!("127.0.0.1:{}", cli.port);
    if let Err(e) = camber::runtime::builder()
        .shutdown_timeout(std::time::Duration::from_secs(1))
        .run(move || run_server(&addr, router))
    {
        let _ = writeln!(std::io::stderr(), "runtime error: {e}");
        std::process::exit(1);
    }
}

fn run_server(addr: &str, router: camber::http::Router) {
    let listener = match camber::net::listen(addr) {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "listen error: {e}");
            std::process::exit(1);
        }
    };
    println!("ready");
    let _ = std::io::stdout().flush();
    let _ = camber::http::serve_listener(listener, router);
}
