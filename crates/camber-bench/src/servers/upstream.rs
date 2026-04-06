use crate::error::BenchError;
use std::net::SocketAddr;
use std::sync::LazyLock;
use std::time::Duration;

const UPSTREAM_BODY: &str = r#"{"status":"ok"}"#;
const SIMULATED_LATENCY: Duration = Duration::from_millis(1);

static UPSTREAM_RESPONSE: LazyLock<Box<[u8]>> = LazyLock::new(|| {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{UPSTREAM_BODY}",
        UPSTREAM_BODY.len(),
    )
    .into_bytes()
    .into_boxed_slice()
});

/// Start a mock upstream HTTP server on a random port. Returns the bound address.
/// Uses a dedicated multi-thread runtime to handle high concurrency without backpressure.
pub fn start() -> Result<SocketAddr, BenchError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    listener.set_nonblocking(true)?;

    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(accept_loop(listener));
    });

    std::thread::sleep(Duration::from_millis(50));
    Ok(addr)
}

/// Bind the upstream mock on a specific port and return the listener.
/// Used by the standalone binary to separate binding from serving.
pub fn bind_on_port(port: u16) -> Result<std::net::TcpListener, BenchError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Run the upstream accept loop on an already-bound listener, blocking forever.
pub fn run_listener(listener: std::net::TcpListener) -> Result<(), BenchError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    rt.block_on(accept_loop(listener));
    Ok(())
}

async fn accept_loop(listener: std::net::TcpListener) {
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
        return;
    };
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        tokio::spawn(handle_connection(stream));
    }
}

async fn handle_connection(mut stream: tokio::net::TcpStream) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = [0u8; 4096];

    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }

        tokio::time::sleep(SIMULATED_LATENCY).await;

        match stream.write_all(&UPSTREAM_RESPONSE).await {
            Ok(()) => {}
            Err(_) => return,
        }
    }
}
