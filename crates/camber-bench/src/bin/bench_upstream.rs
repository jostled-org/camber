use std::io::Write;

#[derive(clap::Parser)]
#[command(name = "bench-upstream", about = "Standalone upstream mock server")]
struct Cli {
    /// Port to listen on
    #[arg(long)]
    port: u16,
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();

    let listener = match camber_bench::servers::upstream::bind_on_port(cli.port) {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "upstream error: {e}");
            std::process::exit(1);
        }
    };

    println!("ready");

    match camber_bench::servers::upstream::run_listener(listener) {
        Ok(()) => {}
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "upstream error: {e}");
            std::process::exit(1);
        }
    }
}
