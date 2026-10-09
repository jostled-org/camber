use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::support::generated_project::{
    camber_command, camber_new, cargo_succeeds, cargo_target_dir, in_temp_dir, patch_local_crates,
    target_directory,
};
use crate::support::process::ChildGuard;
use crate::support::{FixtureError, run_command};
use crate::websocket_peer::exchange_text;

/// The serving expression the advanced template ships. The runtime proof
/// replaces exactly this, because the fixture cannot own a fixed port.
const SHIPPED_SERVE: &str = "http::serve(\"0.0.0.0:8080\", router)";
const ANNOUNCEMENT: &str = "generated app listening on ";
const ECHOED: &str = "hello from the fixture";
/// Bounds a built binary's startup, not its build.
const ANNOUNCEMENT_TIMEOUT: Duration = Duration::from_secs(30);

fn cargo_check(project_dir: &Path) -> Result<(), FixtureError> {
    cargo_succeeds(project_dir, &["check"], "the generated project")
}

fn read_file(project_dir: &Path, relative: &str) -> Result<String, FixtureError> {
    Ok(std::fs::read_to_string(project_dir.join(relative))?)
}

fn assert_current_camber_requirement(project_dir: &Path) -> Result<(), FixtureError> {
    let manifest = read_file(project_dir, "Cargo.toml")?;
    assert!(
        manifest.contains("camber = \"0\""),
        "generated manifest should select the current pre-1.0 Camber release: {manifest}"
    );
    assert!(
        !manifest.contains("camber = \"0.1\""),
        "generated manifest retained the obsolete 0.1 requirement"
    );
    Ok(())
}

/// Generate `name` from `template` under `dir` against the local crates, and
/// check the files every template ships. Returns the project directory and
/// its `src/main.rs`.
fn generate(dir: &Path, name: &str, template: &str) -> Result<(PathBuf, Box<str>), FixtureError> {
    let project_dir = dir.join(name);
    camber_new(dir, name, template)?;
    patch_local_crates(&project_dir)?;

    assert!(project_dir.join("Cargo.toml").exists());
    assert!(project_dir.join("src/main.rs").exists());
    assert!(project_dir.join("llms.txt").exists(), "llms.txt missing");
    assert_current_camber_requirement(&project_dir)?;
    let main_rs = read_file(&project_dir, "src/main.rs")?.into_boxed_str();
    Ok((project_dir, main_rs))
}

#[test]
fn http_template_compiles_and_runs() -> Result<(), FixtureError> {
    in_temp_dir(|dir| {
        let (project_dir, main_rs) = generate(dir, "test-http", "http")?;
        assert!(
            main_rs.contains("use_middleware"),
            "http template should demonstrate middleware"
        );
        assert!(
            main_rs.contains("param("),
            "http template should demonstrate path parameters"
        );
        assert!(
            main_rs.contains("http::get(") || main_rs.contains("http::post("),
            "http template should demonstrate outbound HTTP"
        );
        assert!(
            main_rs.contains("async {"),
            "http template should demonstrate async handlers"
        );
        cargo_check(&project_dir)
    })
}

#[test]
fn fanout_template_compiles_and_runs() -> Result<(), FixtureError> {
    in_temp_dir(|dir| {
        let (project_dir, main_rs) = generate(dir, "test-fanout", "fanout")?;
        assert!(
            main_rs.contains("spawn"),
            "fanout template should demonstrate spawn"
        );
        assert!(
            main_rs.contains("spawn_async"),
            "fanout template should demonstrate async fan-out"
        );
        assert!(
            main_rs.contains("http::get(") || main_rs.contains("http::post("),
            "fanout template should demonstrate outbound HTTP"
        );
        cargo_check(&project_dir)
    })
}

#[test]
fn advanced_template_websocket_compiles_and_runs() -> Result<(), FixtureError> {
    in_temp_dir(check_advanced_template)
}

fn check_advanced_template(dir: &Path) -> Result<(), FixtureError> {
    let (project_dir, main_rs) = generate(dir, "test-advanced", "advanced")?;
    assert!(
        main_rs.contains("grpc"),
        "advanced template should demonstrate gRPC"
    );
    assert!(
        main_rs.contains(".ws("),
        "advanced template should demonstrate WebSocket"
    );
    assert!(
        main_rs.contains(".proxy("),
        "advanced template should demonstrate proxy"
    );
    assert!(
        main_rs.contains("async {"),
        "advanced template should demonstrate async handlers"
    );
    assert!(
        main_rs.contains("use_middleware("),
        "advanced template should demonstrate async middleware"
    );
    assert!(
        project_dir.join("build.rs").exists(),
        "advanced template needs build.rs for protobuf"
    );
    let manifest = read_file(&project_dir, "Cargo.toml")?;
    assert!(
        manifest.contains("camber-build = \"0\""),
        "advanced template should select the current pre-1.0 build helper: {manifest}"
    );
    let build_rs = read_file(&project_dir, "build.rs")?;
    assert!(build_rs.contains("std::io::Result<()>"));
    assert!(!build_rs.contains("Box<dyn"));
    let has_proto =
        std::fs::read_dir(project_dir.join("proto"))?.try_fold(false, |found, entry| {
            entry.map(|entry| found || entry.path().extension().is_some_and(|ext| ext == "proto"))
        })?;
    assert!(has_proto, "advanced template should include a .proto file");
    // A build, not a check: the adapted build below then recompiles only the
    // app, because check and build artifacts share no fingerprints.
    cargo_succeeds(
        &project_dir,
        &["build", "--features", "ws,grpc"],
        "the unchanged advanced app",
    )?;
    adapt_to_ephemeral_listener(&project_dir)?;
    cargo_succeeds(
        &project_dir,
        &["build", "--features", "ws,grpc"],
        "the adapted advanced app",
    )?;
    let binary = cargo_target_dir(&project_dir)?
        .join("debug")
        .join("test-advanced");
    exchange_with_generated_app(&binary)
}

/// Replace the shipped serving expression, and nothing else, with an adapter
/// that binds an ephemeral port and announces it.
fn adapt_to_ephemeral_listener(project_dir: &Path) -> Result<(), FixtureError> {
    let main_path = project_dir.join("src/main.rs");
    let main_rs = std::fs::read_to_string(&main_path)?;
    let occurrences = main_rs.matches(SHIPPED_SERVE).count();
    assert_eq!(
        occurrences, 1,
        "the advanced template should ship exactly one `{SHIPPED_SERVE}`"
    );
    let adapter = format!(
        "camber::runtime::run(move || {{\n        \
         let listener = camber::net::listen(\"127.0.0.1:0\")?;\n        \
         println!(\"{ANNOUNCEMENT}{{}}\", listener.local_addr()?);\n        \
         http::serve_listener(listener, router)\n    \
         }})?"
    );
    std::fs::write(&main_path, main_rs.replacen(SHIPPED_SERVE, &adapter, 1))?;
    Ok(())
}

/// Launch the adapted app, exchange one message on its generated WebSocket
/// route, then kill and reap it and prove its port is free again.
fn exchange_with_generated_app(binary: &Path) -> Result<(), FixtureError> {
    let mut child = ChildGuard::spawn_command(Command::new(binary))?;
    let announced = child.wait_for_announcement(ANNOUNCEMENT, ANNOUNCEMENT_TIMEOUT)?;
    match announced.parse::<SocketAddr>() {
        Ok(addr) => {
            let observed = exchange_text(addr, "/ws/echo", ECHOED)
                .and_then(|reply| expect_echo(&reply))
                .and_then(|()| assert_address_held(addr));
            let cleanup = reap(&mut child).and_then(|()| Ok(TcpListener::bind(addr).map(drop)?));
            FixtureError::with_cleanup(observed, cleanup)
        }
        Err(error) => FixtureError::with_cleanup(
            Err(FixtureError::new(format!(
                "announced address `{announced}` did not parse: {error}"
            ))),
            reap(&mut child),
        ),
    }
}

/// Accept only the echo of the one message the exchange sent.
fn expect_echo(reply: &str) -> Result<(), FixtureError> {
    match reply {
        ECHOED => Ok(()),
        other => Err(FixtureError::new(format!(
            "generated WebSocket route answered `{other}`, not the echo"
        ))),
    }
}

/// Kill the app and prove the reap belongs to it.
fn reap(child: &mut ChildGuard) -> Result<(), FixtureError> {
    let child_id = child.id();
    let probe = child
        .take_reap_probe()
        .ok_or_else(|| FixtureError::new("generated app reap probe was taken"))?;
    child.shutdown()?;
    let reaped = probe.wait()?;
    match reaped.child_id() == child_id {
        true => Ok(()),
        false => Err(FixtureError::new(format!(
            "reaped child {} is not the generated app {child_id}",
            reaped.child_id()
        ))),
    }
}

/// The running app still owns its listener.
fn assert_address_held(addr: SocketAddr) -> Result<(), FixtureError> {
    match TcpListener::bind(addr) {
        Ok(_) => Err(FixtureError::new(
            "generated app released its listener early",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Ok(()),
        Err(error) => Err(FixtureError::from(error)),
    }
}

#[test]
fn unknown_template_returns_error() -> Result<(), FixtureError> {
    in_temp_dir(|dir| {
        let output = run_command(camber_command(
            dir,
            &["new", "test-bad", "--template", "nonexistent"],
        ))?;
        assert!(
            !output.status.success(),
            "camber new with unknown template should fail"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("http") && stderr.contains("fanout") && stderr.contains("advanced"),
            "error should list available templates, got: {stderr}"
        );
        Ok(())
    })
}

#[test]
fn rejects_project_name_with_path_separator() -> Result<(), FixtureError> {
    in_temp_dir(|dir| {
        let output = run_command(camber_command(
            dir,
            &["new", "nested/project", "--template", "http"],
        ))?;
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("must not contain path separators"));
        assert!(!dir.join("nested").exists());
        Ok(())
    })
}

#[test]
fn rejects_invalid_cargo_package_name() -> Result<(), FixtureError> {
    in_temp_dir(|dir| {
        let output = run_command(camber_command(
            dir,
            &["new", "bad name", "--template", "http"],
        ))?;
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("is not a valid Cargo package name"));
        assert!(!dir.join("bad name").exists());
        Ok(())
    })
}

#[test]
fn target_directory_reads_a_plain_path() -> Result<(), FixtureError> {
    let metadata = r#"{"packages":[],"target_directory":"/tmp/target","version":1}"#;
    assert_eq!(target_directory(metadata)?, PathBuf::from("/tmp/target"));
    Ok(())
}

#[test]
fn target_directory_refuses_an_escaped_path() {
    let escaped = [
        r#"{"target_directory":"C:\\target"}"#,
        r#"{"target_directory":"a\"b"}"#,
    ];
    for metadata in escaped {
        assert!(
            target_directory(metadata).is_err(),
            "an escaped path must fail, not be read raw: {metadata}"
        );
    }
    assert!(target_directory("{}").is_err());
}
