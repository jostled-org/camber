use std::process::Command;

fn camber_bin() -> &'static str {
    env!("CARGO_BIN_EXE_camber")
}

#[test]
fn context_command_generates_llms_txt() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(camber_bin())
        .args(["context"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "camber context failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let llms_path = dir.path().join("llms.txt");
    assert!(llms_path.exists(), "llms.txt was not created");

    let content = std::fs::read_to_string(&llms_path).unwrap();

    // Key API signatures must be present
    assert!(content.contains("http::serve"), "missing http::serve");
    assert!(content.contains("Router::new"), "missing Router::new");
    assert!(content.contains("router.get("), "missing router.get");
    assert!(content.contains("Response::text"), "missing Response::text");
    assert!(content.contains("Response::json"), "missing Response::json");
    assert!(content.contains("spawn"), "missing spawn");
    assert!(content.contains("channel"), "missing channel");
    assert!(content.contains("Request"), "missing Request");
    assert!(content.contains("RuntimeError"), "missing RuntimeError");

    // Anti-patterns section
    assert!(
        content.contains("Anti-pattern")
            || content.contains("anti-pattern")
            || content.contains("Avoid"),
        "missing anti-patterns section"
    );
}

#[test]
fn context_overwrites_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let llms_path = dir.path().join("llms.txt");

    // Write stale content
    std::fs::write(&llms_path, "stale content from previous version").unwrap();

    let output = Command::new(camber_bin())
        .args(["context"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "camber context failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let content = std::fs::read_to_string(&llms_path).unwrap();
    assert!(
        !content.contains("stale content"),
        "file was not overwritten"
    );
    assert!(
        content.contains("http::serve"),
        "fresh content missing http::serve"
    );
}
