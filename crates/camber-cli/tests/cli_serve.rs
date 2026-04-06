use camber_cli::config::Config;
use std::io::Write;
use std::process::{Child, Command};
use tempfile::NamedTempFile;

fn write_config(toml: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("temp file");
    f.write_all(toml.as_bytes()).expect("write");
    f
}

/// Find an available TCP port by binding then releasing.
fn available_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("local_addr").port()
}

/// Path to the compiled camber binary.
fn camber_bin() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    // test binary is in target/debug/deps/..., go up to target/debug/
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("camber");
    path
}

/// Spawn `camber serve <config>` and return the child process.
fn spawn_camber_serve(config_path: &std::path::Path) -> Child {
    Command::new(camber_bin())
        .args(["serve", &config_path.to_string_lossy()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn camber serve")
}

/// Poll a URL until it returns a response or timeout.
fn wait_for_ready(url: &str) {
    for _ in 0..50 {
        match std::net::TcpStream::connect_timeout(
            &url.parse().expect("parse addr"),
            std::time::Duration::from_millis(50),
        ) {
            Ok(_) => return,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    panic!("server did not become ready at {url}");
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Bind a backend listener on an available port.
fn spawn_backend() -> (std::net::TcpListener, u16) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let port = listener.local_addr().expect("backend addr").port();
    (listener, port)
}

/// Accept one connection on a backend listener and respond with the given body.
fn serve_one(listener: std::net::TcpListener, body: &'static str) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            use std::io::Read;
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    })
}

/// Send an HTTP/1.1 request with a Host header and return the raw response.
fn raw_request(addr: &str, host: &str, path: &str) -> String {
    use std::io::Read;
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write");
    let mut buf = String::new();
    stream.read_to_string(&mut buf).expect("read");
    buf
}

/// Send an HTTP/1.1 request with a custom method, Host header, and return the raw response.
fn raw_method_request(addr: &str, method: &str, host: &str, path: &str) -> String {
    use std::io::Read;
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    let req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write");
    let mut buf = String::new();
    stream.read_to_string(&mut buf).expect("read");
    buf
}

/// Accept multiple connections on a backend listener and respond to each with the given body.
fn serve_many(
    listener: std::net::TcpListener,
    body: &'static str,
    count: usize,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for _ in 0..count {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Read;
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        }
    })
}

#[test]
fn parse_minimal_config() {
    let f = write_config(
        r#"
listen = ":8443"

[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    assert_eq!(config.listen(), ":8443");
    assert_eq!(config.sites().len(), 1);
    assert_eq!(config.sites()[0].host(), "app.example.com");
    assert_eq!(config.sites()[0].proxy(), Some("http://localhost:3000"));
    assert_eq!(config.sites()[0].root(), None);
    assert!(config.tls().is_none());
}

#[test]
fn parse_full_config() {
    let f = write_config(
        r#"
listen = ":443"

[tls]
cert = "/etc/camber/cert.pem"
key = "/etc/camber/key.pem"

[[site]]
host = "blog.example.com"
proxy = "http://localhost:3000"

[[site]]
host = "docs.example.com"
root = "/var/www/docs"

[[site]]
host = "app.example.com"
proxy = "http://localhost:8080"
root = "/var/www/static"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    assert_eq!(config.listen(), ":443");

    let tls = config.tls().unwrap();
    assert_eq!(tls.cert(), Some("/etc/camber/cert.pem"));
    assert_eq!(tls.key(), Some("/etc/camber/key.pem"));

    assert_eq!(config.sites().len(), 3);

    assert_eq!(config.sites()[0].host(), "blog.example.com");
    assert_eq!(config.sites()[0].proxy(), Some("http://localhost:3000"));
    assert_eq!(config.sites()[0].root(), None);

    assert_eq!(config.sites()[1].host(), "docs.example.com");
    assert_eq!(config.sites()[1].proxy(), None);
    assert_eq!(config.sites()[1].root(), Some("/var/www/docs"));

    assert_eq!(config.sites()[2].host(), "app.example.com");
    assert_eq!(config.sites()[2].proxy(), Some("http://localhost:8080"));
    assert_eq!(config.sites()[2].root(), Some("/var/www/static"));
}

#[test]
fn parse_config_rejects_site_without_proxy_or_root() {
    let f = write_config(
        r#"
[[site]]
host = "empty.example.com"
"#,
    );

    let err = Config::load(f.path()).unwrap_err();
    assert!(
        err.contains("empty.example.com"),
        "error should name the offending host: {err}"
    );
}

#[test]
fn parse_config_default_listen_address() {
    let f = write_config(
        r#"
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    assert_eq!(config.listen(), "0.0.0.0:8080");
}

#[test]
fn camber_serve_proxies_to_backend() {
    // Start a minimal HTTP backend with std::net
    let backend = std::net::TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let backend_port = backend.local_addr().expect("backend addr").port();
    let backend_thread = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = backend.accept() {
            use std::io::Read;
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response =
                "HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nfrom-backend";
            let _ = stream.write_all(response.as_bytes());
        }
    });

    let port = available_port();
    let f = write_config(&format!(
        r#"
listen = "127.0.0.1:{port}"

[[site]]
host = "app.test"
proxy = "http://127.0.0.1:{backend_port}"
"#,
    ));

    let mut child = spawn_camber_serve(f.path());
    wait_for_ready(&format!("127.0.0.1:{port}"));

    // Send request through the proxy with Host header
    use std::io::Read;
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    stream
        .write_all(
            format!("GET /hello HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .expect("write");
    let mut buf = String::new();
    stream.read_to_string(&mut buf).expect("read");

    assert!(
        buf.contains("200") || buf.contains("from-backend"),
        "expected proxied response, got: {buf}"
    );
    assert!(buf.contains("from-backend"), "body missing: {buf}");

    kill(&mut child);
    backend_thread.join().expect("backend thread");
}

#[test]
fn camber_serve_serves_static_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let index_path = dir.path().join("index.html");
    std::fs::write(&index_path, "<h1>hello</h1>").expect("write index");

    let port = available_port();
    let root = dir.path().to_string_lossy();
    let f = write_config(&format!(
        r#"
listen = "127.0.0.1:{port}"

[[site]]
host = "static.test"
root = "{root}"
"#,
    ));

    let mut child = spawn_camber_serve(f.path());
    wait_for_ready(&format!("127.0.0.1:{port}"));

    // Request the static file with Host header
    use std::io::Read;
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    stream
        .write_all(
            format!("GET /index.html HTTP/1.1\r\nHost: static.test\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .expect("write");
    let mut buf = String::new();
    stream.read_to_string(&mut buf).expect("read");

    assert!(buf.contains("200"), "expected 200, got: {buf}");
    assert!(buf.contains("<h1>hello</h1>"), "body missing: {buf}");

    let root_resp = raw_request(&format!("127.0.0.1:{port}"), "static.test", "/");
    assert!(
        root_resp.contains("200"),
        "expected 200 at /, got: {root_resp}"
    );
    assert!(
        root_resp.contains("<h1>hello</h1>"),
        "expected index.html content at /: {root_resp}"
    );

    kill(&mut child);
}

#[test]
fn parse_auto_tls_config() {
    let f = write_config(
        r#"
listen = ":443"

[tls]
auto = true
email = "admin@example.com"

[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    let tls = config.tls().unwrap();
    assert!(tls.auto());
    assert_eq!(tls.email(), Some("admin@example.com"));
    assert!(tls.cert().is_none());
    assert!(tls.key().is_none());
}

#[test]
fn auto_tls_rejects_missing_email() {
    let f = write_config(
        r#"
listen = ":443"

[tls]
auto = true

[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    );

    let err = Config::load(f.path()).unwrap_err();
    assert!(err.contains("email"), "error should mention email: {err}");
}

#[test]
fn auto_tls_rejects_combined_with_manual_cert() {
    let f = write_config(
        r#"
listen = ":443"

[tls]
auto = true
email = "admin@example.com"
cert = "/etc/camber/cert.pem"
key = "/etc/camber/key.pem"

[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    );

    let err = Config::load(f.path()).unwrap_err();
    assert!(
        err.contains("mutually exclusive") || err.contains("auto") && err.contains("cert"),
        "error should mention conflict: {err}"
    );
}

#[test]
fn auto_tls_collects_domains_from_sites() {
    let f = write_config(
        r#"
listen = ":443"

[tls]
auto = true
email = "admin@example.com"

[[site]]
host = "example.com"
proxy = "http://localhost:3000"

[[site]]
host = "api.example.com"
proxy = "http://localhost:8080"

[[site]]
host = "docs.example.com"
root = "/var/www/docs"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    let domains = config.auto_tls_domains();
    assert_eq!(domains.len(), 3);
    assert!(domains.contains(&"example.com"));
    assert!(domains.contains(&"api.example.com"));
    assert!(domains.contains(&"docs.example.com"));
}

#[test]
fn multi_host_proxy_with_static_files() {
    // Two proxy backends
    let (backend_a, port_a) = spawn_backend();
    let (backend_b, port_b) = spawn_backend();
    let thread_a = serve_one(backend_a, "from-a");
    let thread_b = serve_one(backend_b, "from-b");

    // Static file directory
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("index.html"), "<h1>static</h1>").expect("write index");
    let root = dir.path().to_string_lossy();

    // Config: three hosts — two proxies, one static
    let port = available_port();
    let f = write_config(&format!(
        r#"
listen = "127.0.0.1:{port}"

[[site]]
host = "a.test"
proxy = "http://127.0.0.1:{port_a}"

[[site]]
host = "b.test"
proxy = "http://127.0.0.1:{port_b}"

[[site]]
host = "static.test"
root = "{root}"
"#,
    ));

    let mut child = spawn_camber_serve(f.path());
    let addr = format!("127.0.0.1:{port}");
    wait_for_ready(&addr);

    // Host: a.test → proxied to backend-a
    let resp_a = raw_request(&addr, "a.test", "/hello");
    assert!(resp_a.contains("200"), "a.test expected 200: {resp_a}");
    assert!(resp_a.contains("from-a"), "a.test body missing: {resp_a}");

    // Host: b.test → proxied to backend-b
    let resp_b = raw_request(&addr, "b.test", "/hello");
    assert!(resp_b.contains("200"), "b.test expected 200: {resp_b}");
    assert!(resp_b.contains("from-b"), "b.test body missing: {resp_b}");

    // Host: static.test → static file
    let resp_s = raw_request(&addr, "static.test", "/index.html");
    assert!(resp_s.contains("200"), "static.test expected 200: {resp_s}");
    assert!(
        resp_s.contains("<h1>static</h1>"),
        "static.test body missing: {resp_s}"
    );

    // Host: unknown.test → 404
    let resp_u = raw_request(&addr, "unknown.test", "/anything");
    assert!(
        resp_u.contains("404"),
        "unknown.test expected 404: {resp_u}"
    );

    kill(&mut child);
    thread_a.join().expect("backend-a thread");
    thread_b.join().expect("backend-b thread");
}

#[test]
fn config_parses_health_check_fields() {
    let f = write_config(
        r#"
listen = ":8080"

[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
health_check = "/health"
health_interval = 5

[[site]]
host = "static.example.com"
root = "/var/www/html"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    assert_eq!(config.sites().len(), 2);

    // Site with health check configured
    assert_eq!(config.sites()[0].health_check(), Some("/health"));
    assert_eq!(config.sites()[0].health_interval(), Some(5));

    // Site without health check — backward compatible
    assert_eq!(config.sites()[1].health_check(), None);
    assert_eq!(config.sites()[1].health_interval(), None);
}

#[test]
fn config_parses_without_health_check() {
    let f = write_config(
        r#"
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    assert_eq!(config.sites()[0].health_check(), None);
    assert_eq!(config.sites()[0].health_interval(), None);
}

#[test]
fn config_parses_dns_provider_with_env_token() {
    let f = write_config(
        r#"
listen = ":443"

[tls]
auto = true
email = "admin@example.com"
dns_provider = "cloudflare"
dns_api_token_env = "CF_TOKEN"

[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    let tls = config.tls().unwrap();
    assert!(tls.auto());
    assert_eq!(tls.dns_provider(), Some("cloudflare"));
    assert_eq!(tls.dns_api_token_env(), Some("CF_TOKEN"));
    assert!(tls.dns_api_token_file().is_none());
}

#[test]
fn config_parses_dns_provider_with_file_token() {
    let f = write_config(
        r#"
listen = ":443"

[tls]
auto = true
email = "admin@example.com"
dns_provider = "cloudflare"
dns_api_token_file = "/etc/camber/cf.token"

[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    let tls = config.tls().unwrap();
    assert!(tls.auto());
    assert_eq!(tls.dns_provider(), Some("cloudflare"));
    assert!(tls.dns_api_token_env().is_none());
    assert_eq!(tls.dns_api_token_file(), Some("/etc/camber/cf.token"));
}

#[test]
fn auto_tls_without_dns_provider_is_valid() {
    let f = write_config(
        r#"
listen = ":443"

[tls]
auto = true
email = "admin@example.com"

[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    let tls = config.tls().unwrap();
    assert!(tls.auto());
    assert!(tls.dns_provider().is_none());
}

#[test]
fn config_rejects_dns_provider_without_token() {
    let f = write_config(
        r#"
listen = ":443"

[tls]
auto = true
email = "admin@example.com"
dns_provider = "cloudflare"

[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    );

    let err = Config::load(f.path()).unwrap_err();
    assert!(err.contains("token"), "error should mention token: {err}");
}

#[test]
fn config_rejects_both_token_env_and_file() {
    let f = write_config(
        r#"
listen = ":443"

[tls]
auto = true
email = "admin@example.com"
dns_provider = "cloudflare"
dns_api_token_env = "CF_TOKEN"
dns_api_token_file = "/etc/camber/cf.token"

[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    );

    let err = Config::load(f.path()).unwrap_err();
    assert!(
        err.contains("mutually exclusive")
            || (err.contains("dns_api_token_env") && err.contains("dns_api_token_file")),
        "error should mention mutual exclusion: {err}"
    );
}

#[test]
fn parse_config_reads_connection_limit() {
    let f = write_config(
        r#"
connection_limit = 100

[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    assert_eq!(config.connection_limit(), Some(100));
}

#[test]
fn parse_config_connection_limit_defaults_to_none() {
    let f = write_config(
        r#"
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    );

    let config = Config::load(f.path()).unwrap();
    assert_eq!(config.connection_limit(), None);
}

#[test]
fn parse_config_rejects_zero_connection_limit() {
    let f = write_config(
        r#"
connection_limit = 0

[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    );

    let err = Config::load(f.path()).unwrap_err();
    assert_eq!(err, "connection_limit must be at least 1");
}

#[test]
fn parse_config_rejects_zero_health_interval() {
    let f = write_config(
        r#"
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
health_check = "/health"
health_interval = 0
"#,
    );

    let err = Config::load(f.path()).unwrap_err();
    assert_eq!(
        err,
        "site \"app.example.com\" health_interval must be at least 1"
    );
}

#[test]
fn cli_overlay_serves_index_html_at_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("index.html"), "<h1>home</h1>").expect("write index");

    let (backend, backend_port) = spawn_backend();
    let _backend_thread = serve_one(backend, "from-backend");

    let port = available_port();
    let root = dir.path().to_string_lossy();
    let f = write_config(&format!(
        r#"
listen = "127.0.0.1:{port}"

[[site]]
host = "overlay.test"
proxy = "http://127.0.0.1:{backend_port}"
root = "{root}"
"#,
    ));

    let mut child = spawn_camber_serve(f.path());
    wait_for_ready(&format!("127.0.0.1:{port}"));

    let resp = raw_request(&format!("127.0.0.1:{port}"), "overlay.test", "/");
    assert!(resp.contains("200"), "expected 200: {resp}");
    assert!(
        resp.contains("<h1>home</h1>"),
        "expected index.html content at /: {resp}"
    );

    kill(&mut child);
}

#[test]
fn cli_overlay_proxies_root_when_no_index_html() {
    let dir = tempfile::tempdir().expect("tempdir");
    // No index.html — root should fall back to proxy

    let (backend, backend_port) = spawn_backend();
    let backend_thread = serve_one(backend, "proxy-root");

    let port = available_port();
    let root = dir.path().to_string_lossy();
    let f = write_config(&format!(
        r#"
listen = "127.0.0.1:{port}"

[[site]]
host = "overlay.test"
proxy = "http://127.0.0.1:{backend_port}"
root = "{root}"
"#,
    ));

    let mut child = spawn_camber_serve(f.path());
    wait_for_ready(&format!("127.0.0.1:{port}"));

    let resp = raw_request(&format!("127.0.0.1:{port}"), "overlay.test", "/");
    assert!(resp.contains("200"), "expected 200: {resp}");
    assert!(
        resp.contains("proxy-root"),
        "expected proxied response at /: {resp}"
    );

    kill(&mut child);
    backend_thread.join().expect("backend thread");
}

#[test]
fn camber_serve_prefers_local_file_for_existing_get_asset() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("style.css"), "body{color:red}").expect("write css");

    let (backend, backend_port) = spawn_backend();
    let _backend_thread = serve_one(backend, "from-backend");

    let port = available_port();
    let root = dir.path().to_string_lossy();
    let f = write_config(&format!(
        r#"
listen = "127.0.0.1:{port}"

[[site]]
host = "overlay.test"
proxy = "http://127.0.0.1:{backend_port}"
root = "{root}"
"#,
    ));

    let mut child = spawn_camber_serve(f.path());
    wait_for_ready(&format!("127.0.0.1:{port}"));

    let resp = raw_request(&format!("127.0.0.1:{port}"), "overlay.test", "/style.css");
    assert!(resp.contains("200"), "expected 200: {resp}");
    assert!(
        resp.contains("body{color:red}"),
        "expected local file content: {resp}"
    );

    kill(&mut child);
}

#[test]
fn camber_serve_proxies_missing_get_path_when_local_file_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    // No files in the directory — GET should fall back to proxy

    let (backend, backend_port) = spawn_backend();
    let backend_thread = serve_one(backend, "proxy-fallback");

    let port = available_port();
    let root = dir.path().to_string_lossy();
    let f = write_config(&format!(
        r#"
listen = "127.0.0.1:{port}"

[[site]]
host = "overlay.test"
proxy = "http://127.0.0.1:{backend_port}"
root = "{root}"
"#,
    ));

    let mut child = spawn_camber_serve(f.path());
    wait_for_ready(&format!("127.0.0.1:{port}"));

    let resp = raw_request(&format!("127.0.0.1:{port}"), "overlay.test", "/api/data");
    assert!(resp.contains("200"), "expected 200: {resp}");
    assert!(
        resp.contains("proxy-fallback"),
        "expected proxied response: {resp}"
    );

    kill(&mut child);
    backend_thread.join().expect("backend thread");
}

#[test]
fn camber_serve_proxies_non_get_requests_even_when_root_is_present() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Place a file that would match the POST path if it were a GET
    std::fs::write(dir.path().join("submit"), "local-file").expect("write file");

    let (backend, backend_port) = spawn_backend();
    let backend_thread = serve_one(backend, "post-response");

    let port = available_port();
    let root = dir.path().to_string_lossy();
    let f = write_config(&format!(
        r#"
listen = "127.0.0.1:{port}"

[[site]]
host = "overlay.test"
proxy = "http://127.0.0.1:{backend_port}"
root = "{root}"
"#,
    ));

    let mut child = spawn_camber_serve(f.path());
    wait_for_ready(&format!("127.0.0.1:{port}"));

    let resp = raw_method_request(
        &format!("127.0.0.1:{port}"),
        "POST",
        "overlay.test",
        "/submit",
    );
    assert!(resp.contains("200"), "expected 200: {resp}");
    assert!(
        resp.contains("post-response"),
        "POST should go to proxy: {resp}"
    );

    kill(&mut child);
    backend_thread.join().expect("backend thread");
}

#[test]
fn camber_serve_applies_connection_limit_from_config() {
    let (backend, backend_port) = spawn_backend();
    let backend_thread = serve_many(backend, "limited", 2);

    let port = available_port();
    let f = write_config(&format!(
        r#"
listen = "127.0.0.1:{port}"
connection_limit = 1

[[site]]
host = "limit.test"
proxy = "http://127.0.0.1:{backend_port}"
"#,
    ));

    let mut child = spawn_camber_serve(f.path());
    let addr = format!("127.0.0.1:{port}");
    wait_for_ready(&addr);

    // Hold one connection open (keep-alive, no Connection: close)
    use std::io::Read;
    let mut conn1 = std::net::TcpStream::connect(&addr).expect("connect first");
    conn1
        .write_all(b"GET /first HTTP/1.1\r\nHost: limit.test\r\n\r\n")
        .expect("write first");
    // Read the response header to confirm it connected
    let mut header_buf = [0u8; 512];
    let _ = conn1.read(&mut header_buf);

    // Second connection should block until first is released
    let addr2 = addr.clone();
    let second = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let resp = raw_request(&addr2, "limit.test", "/second");
        (start.elapsed(), resp)
    });

    // Give the second connection time to block
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Release the first connection
    drop(conn1);

    let (elapsed, resp) = second.join().expect("second thread");
    assert!(resp.contains("200"), "second request expected 200: {resp}");
    assert!(
        elapsed >= std::time::Duration::from_millis(100),
        "second request should have waited for the slot: {elapsed:?}"
    );

    kill(&mut child);
    backend_thread.join().expect("backend thread");
}

#[test]
fn cli_proxy_health_check_returns_503_before_first_interval_when_upstream_starts_unhealthy() {
    // Backend that immediately refuses connections (bind + drop).
    let dead_port = available_port();

    let port = available_port();
    let f = write_config(&format!(
        r#"
listen = "127.0.0.1:{port}"

[[site]]
host = "sick.test"
proxy = "http://127.0.0.1:{dead_port}"
health_check = "/health"
health_interval = 300
"#,
    ));

    let mut child = spawn_camber_serve(f.path());
    wait_for_ready(&format!("127.0.0.1:{port}"));

    // First request should get 503 — initial probe found the backend dead.
    let resp = raw_request(&format!("127.0.0.1:{port}"), "sick.test", "/anything");
    assert!(
        resp.contains("503"),
        "expected 503 for unhealthy backend, got: {resp}"
    );

    kill(&mut child);
}
