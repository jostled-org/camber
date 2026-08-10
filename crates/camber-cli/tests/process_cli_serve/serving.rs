use std::path::{Path, PathBuf};

use crate::support::FixtureError;
use crate::support::http::{
    Backend, HttpResponse, connect_unix, read_response, request_unix, write_request,
};
use crate::support::process::{ChildGuard, ReadinessTarget, ReapProbe};

struct ServeFixture {
    child: ChildGuard,
    socket_path: PathBuf,
    root: tempfile::TempDir,
}

impl ServeFixture {
    fn start(config_body: &str) -> Result<Self, FixtureError> {
        let root = tempfile::tempdir()?;
        let socket_path = root.path().join("camber.sock");
        let config_path = root.path().join("camber.toml");
        std::fs::write(
            &config_path,
            format!("listen = \"unix:{}\"\n{config_body}", socket_path.display()),
        )?;
        let readiness = ReadinessTarget::Unix(socket_path.clone());
        let mut child = ChildGuard::spawn(
            Path::new(env!("CARGO_BIN_EXE_camber")),
            &config_path,
            readiness,
        )?;
        child.wait_until_ready()?;
        Ok(Self {
            child,
            socket_path,
            root,
        })
    }

    fn request(&self, host: &str, path: &str) -> Result<HttpResponse, FixtureError> {
        self.method_request("GET", host, path)
    }

    fn method_request(
        &self,
        method: &str,
        host: &str,
        path: &str,
    ) -> Result<HttpResponse, FixtureError> {
        Ok(request_unix(&self.socket_path, method, host, path)?)
    }

    fn connect(&self) -> Result<std::os::unix::net::UnixStream, FixtureError> {
        Ok(connect_unix(&self.socket_path)?)
    }

    fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    fn child_id(&self) -> u32 {
        self.child.id()
    }

    fn take_reap_probe(&mut self) -> Result<ReapProbe, FixtureError> {
        self.child
            .take_reap_probe()
            .ok_or_else(|| FixtureError::new("serve child reap probe was absent"))
    }

    fn shutdown(mut self) -> Result<(), FixtureError> {
        self.child.shutdown()?;
        assert!(
            std::os::unix::net::UnixStream::connect(&self.socket_path).is_err(),
            "serve child retained its listener at {}",
            self.socket_path.display()
        );
        assert!(
            self.root.path().exists(),
            "fixture root ended before shutdown"
        );
        Ok(())
    }
}

#[test]
fn camber_serve_proxies_to_backend() -> Result<(), FixtureError> {
    let backend = Backend::one("from-backend");
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "app.test"
proxy = "http://{}"
"#,
        backend.addr()
    ))?;
    let response = server.request("app.test", "/hello")?;
    assert_eq!(response.status, 200);
    assert_eq!(&*response.body, "from-backend");
    server.shutdown()?;
    backend.finish()?;
    Ok(())
}

#[test]
fn camber_serve_serves_static_files() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("index.html"), "<h1>hello</h1>")?;
    let root = dir.path().to_string_lossy();
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "static.test"
root = "{root}"
"#
    ))?;
    let file_response = server.request("static.test", "/index.html")?;
    assert_eq!(file_response.status, 200);
    assert_eq!(&*file_response.body, "<h1>hello</h1>");
    let root_response = server.request("static.test", "/")?;
    assert_eq!(root_response.status, 200);
    assert_eq!(&*root_response.body, "<h1>hello</h1>");
    server.shutdown()?;
    Ok(())
}

#[test]
fn multi_host_proxy_with_static_files() -> Result<(), FixtureError> {
    let backend_a = Backend::one("from-a");
    let backend_b = Backend::one("from-b");
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("index.html"), "<h1>static</h1>")?;
    let root = dir.path().to_string_lossy();
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "a.test"
proxy = "http://{}"
[[site]]
host = "b.test"
proxy = "http://{}"
[[site]]
host = "static.test"
root = "{root}"
"#,
        backend_a.addr(),
        backend_b.addr()
    ))?;

    let response_a = server.request("a.test", "/hello")?;
    assert_eq!(response_a.status, 200);
    assert_eq!(&*response_a.body, "from-a");
    let response_b = server.request("b.test", "/hello")?;
    assert_eq!(response_b.status, 200);
    assert_eq!(&*response_b.body, "from-b");
    let static_response = server.request("static.test", "/index.html")?;
    assert_eq!(static_response.status, 200);
    assert_eq!(&*static_response.body, "<h1>static</h1>");
    assert_eq!(server.request("unknown.test", "/anything")?.status, 404);
    server.shutdown()?;
    backend_a.finish()?;
    backend_b.finish()?;
    Ok(())
}

#[test]
fn cli_overlay_serves_index_html_at_root() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("index.html"), "<h1>home</h1>")?;
    let backend = Backend::one("from-backend");
    let root = dir.path().to_string_lossy();
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "overlay.test"
proxy = "http://{}"
root = "{root}"
"#,
        backend.addr()
    ))?;
    let response = server.request("overlay.test", "/")?;
    assert_eq!(response.status, 200);
    assert_eq!(&*response.body, "<h1>home</h1>");
    server.shutdown()?;
    backend.stop()?;
    Ok(())
}

#[test]
fn cli_overlay_proxies_root_when_no_index_html() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    let backend = Backend::one("proxy-root");
    let root = dir.path().to_string_lossy();
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "overlay.test"
proxy = "http://{}"
root = "{root}"
"#,
        backend.addr()
    ))?;
    let response = server.request("overlay.test", "/")?;
    assert_eq!(response.status, 200);
    assert_eq!(&*response.body, "proxy-root");
    server.shutdown()?;
    backend.finish()?;
    Ok(())
}

#[test]
fn camber_serve_prefers_local_file_for_existing_get_asset() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("style.css"), "body{color:red}")?;
    let backend = Backend::one("from-backend");
    let root = dir.path().to_string_lossy();
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "overlay.test"
proxy = "http://{}"
root = "{root}"
"#,
        backend.addr()
    ))?;
    let response = server.request("overlay.test", "/style.css")?;
    assert_eq!(response.status, 200);
    assert_eq!(&*response.body, "body{color:red}");
    server.shutdown()?;
    backend.stop()?;
    Ok(())
}

#[test]
fn camber_serve_proxies_missing_get_path_when_local_file_absent() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    let backend = Backend::one("proxy-fallback");
    let root = dir.path().to_string_lossy();
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "overlay.test"
proxy = "http://{}"
root = "{root}"
"#,
        backend.addr()
    ))?;
    let response = server.request("overlay.test", "/api/data")?;
    assert_eq!(response.status, 200);
    assert_eq!(&*response.body, "proxy-fallback");
    server.shutdown()?;
    backend.finish()?;
    Ok(())
}

#[test]
fn camber_serve_proxies_non_get_requests_even_when_root_is_present() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("submit"), "local-file")?;
    let backend = Backend::one("post-response");
    let root = dir.path().to_string_lossy();
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "overlay.test"
proxy = "http://{}"
root = "{root}"
"#,
        backend.addr()
    ))?;
    let response = server.method_request("POST", "overlay.test", "/submit")?;
    assert_eq!(response.status, 200);
    assert_eq!(&*response.body, "post-response");
    server.shutdown()?;
    backend.finish()?;
    Ok(())
}

#[test]
fn camber_serve_applies_connection_limit_from_config() -> Result<(), FixtureError> {
    let backend = Backend::many("limited", 3);
    let server = ServeFixture::start(&format!(
        r#"
connection_limit = 1
[[site]]
host = "limit.test"
proxy = "http://{}"
"#,
        backend.addr()
    ))?;

    let exercised = exercise_connection_limit(&server);
    match exercised {
        Ok(()) => finish_connection_limit_case(server, backend),
        Err(error) => finish_failed_connection_limit_case(server, backend, error),
    }
}

fn exercise_connection_limit(server: &ServeFixture) -> Result<(), FixtureError> {
    let mut first_connection = server.connect()?;
    write_request(&mut first_connection, "GET", "limit.test", "/first", false)
        .map_err(|error| FixtureError::new(format!("write /first: {error}")))?;
    let first_response = read_response(&mut first_connection)?;
    assert_eq!(first_response.status, 200);
    if first_response.connection_close {
        return Err(FixtureError::new(
            "the first response closed a requested keep-alive connection",
        ));
    }
    let mut second_connection = server.connect()?;
    write_request(&mut second_connection, "GET", "limit.test", "/second", true)
        .map_err(|error| FixtureError::new(format!("write /second: {error}")))?;
    write_request(&mut first_connection, "GET", "limit.test", "/release", true)
        .map_err(|error| FixtureError::new(format!("write /release: {error}")))?;
    assert_eq!(read_response(&mut first_connection)?.status, 200);
    drop(first_connection);
    let second_response = read_response(&mut second_connection)?;
    assert_eq!(second_response.status, 200);
    assert_eq!(&*second_response.body, "limited");
    Ok(())
}

fn finish_connection_limit_case(
    server: ServeFixture,
    backend: Backend,
) -> Result<(), FixtureError> {
    server.shutdown()?;
    let report = backend.finish()?;
    assert!(
        report.request_paths().eq(["/first", "/release", "/second"]),
        "the queued second request reached the backend before the active connection released its slot"
    );
    Ok(())
}

fn finish_failed_connection_limit_case(
    server: ServeFixture,
    backend: Backend,
    failure: FixtureError,
) -> Result<(), FixtureError> {
    let backend_cleanup = backend.stop();
    let server_cleanup = server.shutdown();
    match (backend_cleanup, server_cleanup) {
        (Ok(_), Ok(())) => Err(failure),
        (Err(backend_error), Ok(())) => Err(FixtureError::new(format!(
            "{failure}; backend cleanup failed: {backend_error}"
        ))),
        (Ok(_), Err(server_error)) => Err(FixtureError::new(format!(
            "{failure}; server cleanup failed: {server_error}"
        ))),
        (Err(backend_error), Err(server_error)) => Err(FixtureError::new(format!(
            "{failure}; backend cleanup failed: {backend_error}; server cleanup failed: {server_error}"
        ))),
    }
}

#[test]
fn cli_proxy_health_check_returns_503_before_first_interval_when_upstream_starts_unhealthy()
-> Result<(), FixtureError> {
    let backend = Backend::unhealthy();
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "sick.test"
proxy = "http://{}"
health_check = "/health"
health_interval = 300
"#,
        backend.addr()
    ))?;
    backend.finish()?;
    assert_eq!(server.request("sick.test", "/anything")?.status, 503);
    server.shutdown()?;
    Ok(())
}

#[test]
fn serve_fixture_reaps_child_within_deadline_after_assertion_panic() -> Result<(), FixtureError> {
    let mut socket_path = None;
    let mut child_id = 0;
    let mut reap_probe = None;
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut server = ServeFixture::start(
            r#"
[[site]]
host = "panic.test"
root = "/tmp"
"#,
        )
        .map_err(|error| error.to_string())?;
        socket_path = Some(server.socket_path().to_path_buf());
        child_id = server.child_id();
        reap_probe = Some(
            server
                .take_reap_probe()
                .map_err(|error| error.to_string())?,
        );
        assert_eq!(std::process::id(), 0, "simulated assertion failure");
        Ok::<(), String>(())
    }));
    assert!(panic_result.is_err());
    assert_ne!(child_id, 0, "serve child did not start");
    let reaped = reap_probe
        .ok_or_else(|| FixtureError::new("reap probe was not retained across panic"))?
        .wait()?;
    assert_eq!(reaped.child_id(), child_id);
    assert!(
        !reaped.status().success(),
        "serve child exited successfully"
    );
    let socket_path = socket_path
        .ok_or_else(|| FixtureError::new("serve socket path was not retained across panic"))?;
    assert!(
        std::os::unix::net::UnixStream::connect(&socket_path).is_err(),
        "serve child retained its listener after fixture drop"
    );
    Ok(())
}
