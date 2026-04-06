// ── HTTP template (default) ──────────────────────────────────────────

pub const HTTP_CARGO_TOML: &str = r#"[package]
name = "{{name}}"
version = "0.1.0"
edition = "2024"

[dependencies]
camber = "0.1"
"#;

pub const HTTP_MAIN_RS: &str = r#"use camber::RuntimeError;
use camber::http::{self, Request, Response, Router};

fn main() -> Result<(), RuntimeError> {
    let mut router = Router::new();

    // Middleware: log request timing
    router.use_middleware(|req, next| {
        let method = req.method().to_owned();
        let path = req.path().to_owned();
        let start = std::time::Instant::now();
        let fut = next.call(req);
        Box::pin(async move {
            let resp = fut.await;
            println!("{method} {path} {}ms", start.elapsed().as_millis());
            resp
        })
    });

    router.get("/hello", |_req: &Request| async {
        Response::text(200, "Hello, world!")
    });

    router.get("/users/:id", |req: &Request| {
        let id = req.param("id").map(str::to_owned);
        async move {
            match id {
                Some(id) => Response::text(200, &format!("User {id}")),
                None => Response::text(400, "missing id"),
            }
        }
    });

    router.get("/proxy", |_req: &Request| async {
        match http::get("https://httpbin.org/get").await {
            Ok(resp) => Response::text(200, resp.body()),
            Err(_) => Response::text(502, "upstream error"),
        }
    });

    http::serve("0.0.0.0:8080", router)
}
"#;

// ── Fan-out CLI template ────────────────────────────────────────────

pub const FANOUT_CARGO_TOML: &str = r#"[package]
name = "{{name}}"
version = "0.1.0"
edition = "2024"

[dependencies]
camber = "0.1"
"#;

pub const FANOUT_MAIN_RS: &str = r#"use camber::http;
use camber::{spawn_async, RuntimeError};

fn main() -> Result<(), RuntimeError> {
    camber::runtime::run(|| camber::runtime::block_on(fan_out()))?
}

async fn fan_out() -> Result<(), RuntimeError> {
    let urls = [
        "https://httpbin.org/get",
        "https://httpbin.org/ip",
        "https://httpbin.org/headers",
    ];

    let mut handles = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        let url = url.to_string();
        handles.push(spawn_async(async move {
            let status = match http::get(&url).await {
                Ok(resp) => resp.status(),
                Err(_) => 0,
            };
            (i, status)
        }));
    }

    for handle in handles {
        let (i, status) = handle.await?;
        println!("request {i}: {status}");
    }

    Ok(())
}
"#;

// ── Advanced template (gRPC + WebSocket + proxy) ────────────────────

pub const ADVANCED_CARGO_TOML: &str = r#"[package]
name = "{{name}}"
version = "0.1.0"
edition = "2024"

[features]
default = ["ws", "grpc"]
ws = ["camber/ws"]
grpc = ["camber/grpc"]

[dependencies]
camber = "0.1"
tokio = { version = "1", default-features = false }
tonic = { version = "0.12", default-features = false }
prost = "0.13"

[build-dependencies]
camber-build = "0.1"
"#;

pub const ADVANCED_MAIN_RS: &str = r#"mod proto {
    tonic::include_proto!("echo");
}

use proto::echo_service;

use camber::RuntimeError;
use camber::http::{self, GrpcRouter, Request, Response, Router, WsConn};

fn main() -> Result<(), RuntimeError> {
    let mut router = Router::new();

    // Middleware: log request timing
    router.use_middleware(|req, next| {
        let method = req.method().to_owned();
        let path = req.path().to_owned();
        let start = std::time::Instant::now();
        let fut = next.call(req);
        Box::pin(async move {
            let resp = fut.await;
            println!("{method} {path} {}ms", start.elapsed().as_millis());
            resp
        })
    });

    // REST — async handler
    router.get("/health", |_req: &Request| async {
        Response::text(200, "ok")
    });

    // Middleware — adds timing header
    router.use_middleware(|req, next| {
        let start = std::time::Instant::now();
        let fut = next.call(req);
        Box::pin(async move {
            let resp = fut.await;
            resp.with_header("X-Response-Time-Ms", &start.elapsed().as_millis().to_string())
        })
    });

    // WebSocket echo
    router.ws("/ws/echo", |_req: &Request, mut conn: WsConn| {
        while let Some(msg) = conn.recv() {
            conn.send(&msg)?;
        }
        Ok(())
    });

    // gRPC
    let grpc = GrpcRouter::new().add_service(echo_service::serve(EchoService));
    router.grpc(grpc);

    // Reverse proxy
    router.proxy("/api", "http://localhost:3000");

    http::serve("0.0.0.0:8080", router)
}

struct EchoService;

#[tonic::async_trait]
impl echo_service::Echo for EchoService {
    async fn echo(
        &self,
        request: tonic::Request<proto::EchoRequest>,
    ) -> Result<tonic::Response<proto::EchoReply>, tonic::Status> {
        let msg = request.into_inner().message;
        Ok(tonic::Response::new(proto::EchoReply { message: msg }))
    }
}
"#;

pub const ADVANCED_BUILD_RS: &str = r#"fn main() -> Result<(), Box<dyn std::error::Error>> {
    camber_build::compile_protos(&["proto/echo.proto"], &["proto"])?;
    Ok(())
}
"#;

pub const ADVANCED_PROTO: &str = r#"syntax = "proto3";

package echo;

service Echo {
  rpc Echo (EchoRequest) returns (EchoReply);
}

message EchoRequest {
  string message = 1;
}

message EchoReply {
  string message = 1;
}
"#;

pub const AVAILABLE_TEMPLATES: &[&str] = &["http", "fanout", "advanced"];
