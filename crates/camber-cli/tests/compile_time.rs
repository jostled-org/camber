use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn camber_bin() -> &'static str {
    env!("CARGO_BIN_EXE_camber")
}

fn camber_crate_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent dir")
        .join("camber")
}

fn camber_build_crate_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent dir")
        .join("camber-build")
}

fn patch_local_crates(project_dir: &Path) {
    let config_dir = project_dir.join(".cargo");
    std::fs::create_dir_all(&config_dir).expect("create .cargo dir");

    let patch = format!(
        "[patch.crates-io]\ncamber = {{ path = \"{}\" }}\ncamber-build = {{ path = \"{}\" }}\n",
        camber_crate_path().display(),
        camber_build_crate_path().display(),
    );
    std::fs::write(config_dir.join("config.toml"), patch).expect("write cargo config");
}

// 4.T1: incremental compile time under 5 seconds
#[test]
#[ignore = "developer-loop timing check; not suitable for standard CI"]
fn incremental_compile_under_5_seconds() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let project_dir = dir.path().join("test-incremental");

    // Generate HTTP template project
    let output = Command::new(camber_bin())
        .args(["new", "test-incremental", "--template", "http"])
        .current_dir(dir.path())
        .output()
        .expect("run camber new");
    assert!(
        output.status.success(),
        "camber new failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    patch_local_crates(&project_dir);

    // Clean build first (populates cache)
    let build = Command::new("cargo")
        .args(["build"])
        .current_dir(&project_dir)
        .output()
        .expect("run cargo build");
    assert!(
        build.status.success(),
        "initial build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    // Modify one handler line to trigger incremental recompile
    let main_rs = project_dir.join("src/main.rs");
    let source = std::fs::read_to_string(&main_rs).expect("read main.rs");
    let modified = source.replace("\"Hello, world!\"", "\"Hello, incremental!\"");
    std::fs::write(&main_rs, modified).expect("write modified main.rs");

    // Measure incremental compile time
    let start = Instant::now();
    let incremental = Command::new("cargo")
        .args(["build"])
        .current_dir(&project_dir)
        .output()
        .expect("run incremental build");
    let elapsed = start.elapsed();

    assert!(
        incremental.status.success(),
        "incremental build failed: {}",
        String::from_utf8_lossy(&incremental.stderr)
    );

    assert!(
        elapsed.as_secs() < 5,
        "incremental compile took {elapsed:?}, exceeds 5-second ceiling"
    );
}
