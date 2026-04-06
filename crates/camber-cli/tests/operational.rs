use std::process::Command;

// 4.T2: Dockerfile builds successfully
#[test]
#[ignore] // requires Docker daemon; skip in CI environments without Docker
fn dockerfile_builds_successfully() {
    let project_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parent dir")
        .parent()
        .expect("workspace root");

    assert!(
        project_root.join("Dockerfile").exists(),
        "Dockerfile not found at project root"
    );

    let output = Command::new("docker")
        .args(["build", "."])
        .current_dir(project_root)
        .output()
        .expect("run docker build");

    assert!(
        output.status.success(),
        "docker build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
