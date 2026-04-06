use std::path::{Path, PathBuf};
use std::process::Command;

fn camber_bin() -> &'static str {
    env!("CARGO_BIN_EXE_camber")
}

fn camber_crate_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("camber")
}

fn camber_build_crate_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("camber-build")
}

fn patch_local_crates(project_dir: &Path) {
    let config_dir = project_dir.join(".cargo");
    std::fs::create_dir_all(&config_dir).unwrap();

    let patch = format!(
        "[patch.crates-io]\ncamber = {{ path = \"{}\" }}\ncamber-build = {{ path = \"{}\" }}\n",
        camber_crate_path().display(),
        camber_build_crate_path().display(),
    );
    std::fs::write(config_dir.join("config.toml"), patch).unwrap();
}

fn run_camber_new(dir: &Path, name: &str, template: &str) {
    let output = Command::new(camber_bin())
        .args(["new", name, "--template", template])
        .current_dir(dir)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "camber new --template {template} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cargo_check(project_dir: &Path) {
    let check = Command::new("cargo")
        .args(["check"])
        .current_dir(project_dir)
        .output()
        .unwrap();

    assert!(
        check.status.success(),
        "cargo check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
}

fn cargo_check_features(project_dir: &Path, features: &str) {
    let check = Command::new("cargo")
        .args(["check", "--features", features])
        .current_dir(project_dir)
        .output()
        .unwrap();

    assert!(
        check.status.success(),
        "cargo check --features {features} failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
}

fn read_file(project_dir: &Path, relative: &str) -> String {
    std::fs::read_to_string(project_dir.join(relative)).unwrap()
}

// 3.T1: http template compiles and contains expected patterns
#[test]
fn http_template_compiles_and_runs() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("test-http");

    run_camber_new(dir.path(), "test-http", "http");
    patch_local_crates(&project_dir);

    // Assert files exist
    assert!(project_dir.join("Cargo.toml").exists());
    assert!(project_dir.join("src/main.rs").exists());
    assert!(project_dir.join("llms.txt").exists(), "llms.txt missing");

    // Assert main.rs contains middleware, path params, outbound HTTP
    let main_rs = read_file(&project_dir, "src/main.rs");
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

    cargo_check(&project_dir);
}

// 3.T2: fanout template compiles and contains concurrency patterns
#[test]
fn fanout_template_compiles_and_runs() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("test-fanout");

    run_camber_new(dir.path(), "test-fanout", "fanout");
    patch_local_crates(&project_dir);

    assert!(project_dir.join("Cargo.toml").exists());
    assert!(project_dir.join("src/main.rs").exists());
    assert!(project_dir.join("llms.txt").exists(), "llms.txt missing");

    let main_rs = read_file(&project_dir, "src/main.rs");
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

    cargo_check(&project_dir);
}

// 3.T3: advanced template compiles with ws and grpc features
#[test]
fn advanced_template_compiles() {
    let dir = tempfile::tempdir().unwrap();
    let project_dir = dir.path().join("test-advanced");

    run_camber_new(dir.path(), "test-advanced", "advanced");
    patch_local_crates(&project_dir);

    assert!(project_dir.join("Cargo.toml").exists());
    assert!(project_dir.join("src/main.rs").exists());
    assert!(project_dir.join("llms.txt").exists(), "llms.txt missing");

    let main_rs = read_file(&project_dir, "src/main.rs");
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

    // Proto and build.rs
    assert!(
        project_dir.join("build.rs").exists(),
        "advanced template needs build.rs for protobuf"
    );
    let proto_files: Vec<_> = std::fs::read_dir(project_dir.join("proto"))
        .expect("proto/ directory should exist")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "proto"))
        .collect();
    assert!(
        !proto_files.is_empty(),
        "advanced template should include a .proto file"
    );

    cargo_check_features(&project_dir, "ws,grpc");
}

// 3.T4: unknown template returns error
#[test]
fn unknown_template_returns_error() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(camber_bin())
        .args(["new", "test-bad", "--template", "nonexistent"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "camber new with unknown template should fail"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("http") && stderr.contains("fanout") && stderr.contains("advanced"),
        "error should list available templates, got: {stderr}"
    );
}

#[test]
fn rejects_project_name_with_path_separator() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(camber_bin())
        .args(["new", "nested/project", "--template", "http"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("must not contain path separators"));
    assert!(!dir.path().join("nested").exists());
}

#[test]
fn rejects_invalid_cargo_package_name() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(camber_bin())
        .args(["new", "bad name", "--template", "http"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is not a valid Cargo package name"));
    assert!(!dir.path().join("bad name").exists());
}
