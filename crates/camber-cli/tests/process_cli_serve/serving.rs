use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::scripted_upstream::{ForwardedRequest, ScriptedUpstream};
use crate::support::FixtureError;
use crate::support::http::{
    Backend, HttpResponse, connect_unix, read_response, request_unix, status_code, write_request,
};
use crate::support::process::{ChildGuard, ReadinessTarget, ReapProbe};

/// The declared Step 16 red diagnostic: an overlay fallback forwarded while
/// the site's health authority held the upstream unhealthy.
const OVERLAY_BYPASS: &str = "M9 unhealthy overlay reached the upstream";
/// How long a scripted upstream may take to answer the next probe. The CLI's
/// shortest interval is one second, so this bounds a hang, not a cadence.
const PROBE_BOUND: Duration = Duration::from_secs(10);
/// How long the serve child may take to publish a state its probe already read.
const COMMIT_BOUND: Duration = Duration::from_secs(10);
const COMMIT_POLL: Duration = Duration::from_millis(20);
const HEAD_RESPONSE_LIMIT: u64 = 64 * 1024;

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

    /// Send a HEAD request and return its status. A HEAD response declares a
    /// length it never sends, so this reads to the server's close instead of
    /// to the declared length.
    fn head_status(&self, host: &str, path: &str) -> Result<u16, FixtureError> {
        let mut stream = self.connect()?;
        write_request(&mut stream, "HEAD", host, path, true)?;
        let mut response = Vec::new();
        stream
            .take(HEAD_RESPONSE_LIMIT)
            .read_to_end(&mut response)?;
        status_code(&String::from_utf8(response)?)
            .ok_or_else(|| FixtureError::new(format!("HEAD {path} had no status line")))
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
        self.root.close()?;
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
fn configured_site_ports_preserve_hostname_routing() -> Result<(), FixtureError> {
    for (configured, requests) in [
        (
            "APP.TEST.:8080",
            ["app.test:8080", "app.test", "APP.TEST:9090"],
        ),
        (
            "127.0.0.1:8080",
            ["127.0.0.1:8080", "127.0.0.1", "127.0.0.1:9090"],
        ),
        (
            "[0:0:0:0:0:0:0:1]:8443",
            ["[::1]:8443", "[::1]", "[::1]:9090"],
        ),
    ] {
        serve_port_bearing_site(configured, requests)?;
    }
    Ok(())
}

fn serve_port_bearing_site(configured: &str, requests: [&str; 3]) -> Result<(), FixtureError> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("index.html"), configured)?;
    let server = ServeFixture::start(&format!(
        "[[site]]\nhost = \"{configured}\"\nroot = \"{}\"\n",
        root.path().display()
    ))?;
    let responses = requests.map(|host| server.request(host, "/index.html"));
    let unknown = server.request("unknown.test:8080", "/index.html");
    server.shutdown()?;
    root.close()?;
    for (host, response) in requests.into_iter().zip(responses) {
        let response = response?;
        assert_eq!(
            response.status, 200,
            "configured {configured}, request {host}"
        );
        assert_eq!(
            &*response.body, configured,
            "the configured site must serve"
        );
    }
    assert_eq!(unknown?.status, 404, "the site must not become a fallback");
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
    let overlay_backend = Backend::unhealthy();
    let overlay_root = tempfile::tempdir()?;
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "sick.test"
proxy = "http://{}"
health_check = "/health"
health_interval = 300
[[site]]
host = "sick-overlay.test"
proxy = "http://{}"
root = "{}"
health_check = "/health"
health_interval = 300
"#,
        backend.addr(),
        overlay_backend.addr(),
        overlay_root.path().display()
    ))?;
    // Each backend answers exactly its site's initial probe, so finishing it
    // is the acknowledgement that the probe was answered unhealthy.
    backend.finish()?;
    overlay_backend.finish()?;
    let proxy_only = server.request("sick.test", "/anything");
    let overlay = server.request("sick-overlay.test", "/anything");
    server.shutdown()?;
    overlay_root.close()?;
    assert_eq!(proxy_only?.status, 503);
    let overlay = overlay?;
    assert_eq!(
        overlay.status, 503,
        "{OVERLAY_BYPASS}: a missing GET before the first interval answered {} with {:?}",
        overlay.status, overlay.body
    );
    Ok(())
}

/// One overlay site and one proxy-only site, each with its own scripted
/// upstream behind a path prefix. Both upstreams start unhealthy, recover, and
/// fall again. Each phase begins only once the serve child has published the
/// new state: a proxy-only GET and an overlay POST both answer from the
/// site's health authority, so their refusal or forwarding is the committed
/// state, not the probe the upstream answered.
#[test]
fn overlay_and_proxy_only_share_health_refusal_and_recovery() -> Result<(), FixtureError> {
    let mut proxy_upstream = ScriptedUpstream::start("/proxy-only/health", false)?;
    let mut overlay_upstream = ScriptedUpstream::start("/overlay/health", false)?;
    let site_root = tempfile::tempdir()?;
    std::fs::write(site_root.path().join("style.css"), "body{color:red}")?;
    std::fs::write(site_root.path().join("submit"), "local-file")?;
    let server = ServeFixture::start(&format!(
        r#"
[[site]]
host = "proxy.test"
proxy = "http://{}/proxy-only"
health_check = "/health"
health_interval = 1
[[site]]
host = "overlay.test"
proxy = "http://{}/overlay"
root = "{}"
health_check = "/health"
health_interval = 1
"#,
        proxy_upstream.addr(),
        overlay_upstream.addr(),
        site_root.path().display()
    ))?;

    let mut rows = HealthRows::default();
    let exercised = exercise_shared_health(
        &server,
        &mut proxy_upstream,
        &mut overlay_upstream,
        &mut rows,
    );
    let server_cleanup = server.shutdown();
    let proxy_log = proxy_upstream.finish();
    let overlay_log = overlay_upstream.finish();
    let root_cleanup = site_root.close().map_err(FixtureError::from);

    let mut errors: Vec<String> = Vec::new();
    for (stage, result) in [
        ("exercise", exercised),
        ("serve child teardown", server_cleanup),
        ("site root teardown", root_cleanup),
    ] {
        match result {
            Ok(()) => {}
            Err(error) => errors.push(format!("{stage}: {error}")),
        }
    }
    match (proxy_log, overlay_log) {
        (Ok(proxy_log), Ok(overlay_log)) => rows.check_final_logs(&proxy_log, &overlay_log),
        (proxy_log, overlay_log) => errors.extend(
            [
                ("proxy-only upstream teardown", proxy_log),
                ("overlay upstream teardown", overlay_log),
            ]
            .into_iter()
            .filter_map(|(stage, result)| result.err().map(|error| format!("{stage}: {error}"))),
        ),
    }
    errors.extend(rows.failures);
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    Ok(())
}

/// Failures collected across every row, reported together after teardown.
#[derive(Default)]
struct HealthRows {
    failures: Vec<String>,
}

impl HealthRows {
    fn status(&mut self, row: &str, actual: u16, expected: u16) {
        match actual == expected {
            true => {}
            false => self
                .failures
                .push(format!("{row}: answered {actual}, expected {expected}")),
        }
    }

    fn response(&mut self, row: &str, actual: &HttpResponse, status: u16, body: &str) {
        match (actual.status == status, &*actual.body == body) {
            (true, true) => {}
            _ => self.failures.push(format!(
                "{row}: answered {} with {:?}, expected {status} with {body:?}",
                actual.status, actual.body
            )),
        }
    }

    fn overlay_refused(&mut self, row: &str, actual: u16) {
        match actual {
            503 => {}
            _ => self.failures.push(format!(
                "{OVERLAY_BYPASS}: {row} answered {actual}, expected 503"
            )),
        }
    }

    fn untouched(&mut self, label: &str, forwarded: &[ForwardedRequest], since: usize) {
        let reached: Box<[&ForwardedRequest]> = forwarded
            .iter()
            .skip(since)
            .filter(|request| !is_witness(request))
            .collect();
        match reached.is_empty() {
            true => {}
            false => self
                .failures
                .push(format!("{label} while unhealthy: {reached:?}")),
        }
    }

    fn check_final_logs(
        &mut self,
        proxy_log: &[ForwardedRequest],
        overlay_log: &[ForwardedRequest],
    ) {
        let proxy_expected = [ForwardedRequest::new("GET", "/proxy-only/api/data")];
        let overlay_expected = [
            ForwardedRequest::new("GET", "/overlay/api/data"),
            ForwardedRequest::new("HEAD", "/overlay/api/data"),
            ForwardedRequest::new("POST", "/overlay/submit"),
        ];
        for (label, log, expected) in [
            ("proxy-only upstream", proxy_log, &proxy_expected[..]),
            ("overlay upstream", overlay_log, &overlay_expected[..]),
        ] {
            let observed: Box<[&ForwardedRequest]> =
                log.iter().filter(|request| !is_witness(request)).collect();
            match observed.iter().copied().eq(expected.iter()) {
                true => {}
                false => self.failures.push(format!(
                    "{label} saw {observed:?}; expected only the recovered requests {expected:?}"
                )),
            }
        }
    }
}

/// Commitment witnesses carry this target suffix. They prove when the serve
/// child published a state, so the final logs leave them out.
const WITNESS_SUFFIX: &str = "-witness";

fn is_witness(request: &ForwardedRequest) -> bool {
    request.target.ends_with(WITNESS_SUFFIX)
}

fn exercise_shared_health(
    server: &ServeFixture,
    proxy_upstream: &mut ScriptedUpstream,
    overlay_upstream: &mut ScriptedUpstream,
    rows: &mut HealthRows,
) -> Result<(), FixtureError> {
    // Each site's initial probe is answered before the child binds its
    // listener, so readiness already follows the committed unhealthy state.
    await_committed(server, "GET", "proxy.test", "/start-witness", true)?;
    await_committed(server, "POST", "overlay.test", "/start-witness", true)?;
    exercise_unhealthy(server, rows, "initial")?;
    rows.untouched(OVERLAY_BYPASS, overlay_upstream.forwarded(), 0);
    rows.untouched(
        "the proxy-only site reached its upstream",
        proxy_upstream.forwarded(),
        0,
    );

    for upstream in [&mut *proxy_upstream, &mut *overlay_upstream] {
        upstream.script_health(true);
        upstream.wait_for_probe(200, PROBE_BOUND)?;
    }
    let recovered = await_committed(server, "GET", "proxy.test", "/recovered-witness", false)?;
    rows.response(
        "proxy-only recovery witness",
        &recovered,
        200,
        "upstream GET /proxy-only/recovered-witness",
    );
    let recovered = await_committed(server, "POST", "overlay.test", "/recovered-witness", false)?;
    rows.response(
        "overlay recovery witness",
        &recovered,
        200,
        "upstream POST /overlay/recovered-witness",
    );
    exercise_recovered(server, rows)?;

    let proxy_recovered = proxy_upstream.forwarded().len();
    let overlay_recovered = overlay_upstream.forwarded().len();
    for upstream in [&mut *proxy_upstream, &mut *overlay_upstream] {
        upstream.script_health(false);
        upstream.wait_for_probe(500, PROBE_BOUND)?;
    }
    await_committed(server, "GET", "proxy.test", "/fall-witness", true)?;
    await_committed(server, "POST", "overlay.test", "/fall-witness", true)?;
    exercise_unhealthy(server, rows, "fallen")?;
    rows.untouched(
        OVERLAY_BYPASS,
        overlay_upstream.forwarded(),
        overlay_recovered,
    );
    rows.untouched(
        "the proxy-only site reached its upstream",
        proxy_upstream.forwarded(),
        proxy_recovered,
    );
    Ok(())
}

/// Rows that hold while both sites' authorities hold their upstreams unhealthy.
fn exercise_unhealthy(
    server: &ServeFixture,
    rows: &mut HealthRows,
    phase: &str,
) -> Result<(), FixtureError> {
    let proxy_only = server.request("proxy.test", "/api/data")?;
    rows.status(
        &format!("{phase} unhealthy proxy-only GET /api/data"),
        proxy_only.status,
        503,
    );
    let submit = server.method_request("POST", "overlay.test", "/submit")?;
    rows.status(
        &format!("{phase} unhealthy overlay POST /submit"),
        submit.status,
        503,
    );
    let missing = server.request("overlay.test", "/api/data")?;
    rows.overlay_refused(
        &format!("{phase} overlay missing-file GET /api/data"),
        missing.status,
    );
    let missing_head = server.head_status("overlay.test", "/api/data")?;
    rows.overlay_refused(
        &format!("{phase} overlay missing-file HEAD /api/data"),
        missing_head,
    );
    let missing_index = server.request("overlay.test", "/")?;
    rows.overlay_refused(
        &format!("{phase} overlay missing-index GET /"),
        missing_index.status,
    );
    let local = server.request("overlay.test", "/style.css")?;
    rows.response(
        &format!("{phase} unhealthy overlay local GET /style.css"),
        &local,
        200,
        "body{color:red}",
    );
    let local_head = server.head_status("overlay.test", "/style.css")?;
    rows.status(
        &format!("{phase} unhealthy overlay local HEAD /style.css"),
        local_head,
        200,
    );
    Ok(())
}

/// Rows that hold once both sites' authorities have published recovery.
fn exercise_recovered(server: &ServeFixture, rows: &mut HealthRows) -> Result<(), FixtureError> {
    let proxy_only = server.request("proxy.test", "/api/data")?;
    rows.response(
        "recovered proxy-only GET /api/data keeps its prefix",
        &proxy_only,
        200,
        "upstream GET /proxy-only/api/data",
    );
    let missing = server.request("overlay.test", "/api/data")?;
    rows.response(
        "recovered overlay missing-file GET /api/data keeps its prefix",
        &missing,
        200,
        "upstream GET /overlay/api/data",
    );
    let missing_head = server.head_status("overlay.test", "/api/data")?;
    rows.status(
        "recovered overlay missing-file HEAD /api/data",
        missing_head,
        200,
    );
    let local = server.request("overlay.test", "/style.css")?;
    rows.response(
        "recovered overlay local GET /style.css",
        &local,
        200,
        "body{color:red}",
    );
    let submit = server.method_request("POST", "overlay.test", "/submit")?;
    rows.response(
        "recovered overlay POST /submit streams past the local file",
        &submit,
        200,
        "upstream POST /overlay/submit",
    );
    Ok(())
}

/// Poll one request until the site's health authority answers with the
/// expected state: `503` when `refused`, anything else otherwise.
///
/// The probe acknowledgement only says the upstream answered; the serve child
/// publishes after reading that answer. This request reads the published
/// state itself, so its first matching answer is the commitment.
fn await_committed(
    server: &ServeFixture,
    method: &str,
    host: &str,
    path: &str,
    refused: bool,
) -> Result<HttpResponse, FixtureError> {
    let deadline = Instant::now() + COMMIT_BOUND;
    loop {
        let response = server.method_request(method, host, path)?;
        match (
            (response.status == 503) == refused,
            Instant::now() < deadline,
        ) {
            (true, _) => return Ok(response),
            (false, true) => std::thread::sleep(COMMIT_POLL),
            (false, false) => {
                return Err(FixtureError::new(format!(
                    "{method} {host}{path} did not observe the committed {} state within {COMMIT_BOUND:?}; last answer {}",
                    match refused {
                        true => "unhealthy",
                        false => "recovered",
                    },
                    response.status
                )));
            }
        }
    }
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
