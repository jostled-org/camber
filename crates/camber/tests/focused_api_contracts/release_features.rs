//! Release checks must preserve their verdicts without combining allocators.

use std::collections::BTreeSet;
use std::path::Path;

use crate::delivery_fixture::{
    FixtureRepo, ambient_executable, repository_file, repository_root, run_bounded,
};

const ADAPTER: &str = ".github/scripts/release-tools/cargo-semver-checks";
const API_FEATURES: &str = "profiling,ws,grpc,acme,dns01,nats,sqs,otel";

fn check_adapter(arguments: &[&str], status: i32, configured: bool) -> (i32, Box<str>) {
    let repo = FixtureRepo::new();
    repo.write_executable(
        "semver checker",
        b"#!/bin/bash\nprintf '%s\\n' \"$@\"\nexit \"${CHECKER_STATUS}\"\n",
    );
    let mut command = repo.system_command(Path::new("/bin/bash"));
    command
        .arg(repository_root().join(ADAPTER))
        .args(arguments)
        .env("CHECKER_STATUS", status.to_string());
    if configured {
        command.env(
            "CAMBER_SEMVER_CHECKS_BIN",
            repo.path().join("semver checker"),
        );
    }
    let result = run_bounded(command);
    repo.close();
    (result.status, result.output)
}

#[test]
fn release_checker_selects_api_features_and_preserves_every_verdict() {
    let arguments = [
        "semver-checks",
        "check-release",
        "--release-type",
        "minor",
        "--manifest-path",
        "/tmp/release checkout/crates/camber/Cargo.toml",
        "--package",
        "camber",
        "--baseline-root",
        "/tmp/published baseline/Cargo.toml",
    ];
    let expected = format!(
        "{}\n--default-features\n--features\n{API_FEATURES}\n",
        arguments.join("\n")
    );
    for status in [0, 100, 101] {
        let (actual, output) = check_adapter(&arguments, status, true);
        assert_eq!(actual, status, "{output}");
        assert_eq!(&*output, expected);
    }
}

#[test]
fn release_checker_leaves_other_packages_and_version_queries_unchanged() {
    for arguments in [
        vec!["--version"],
        vec![
            "semver-checks",
            "check-release",
            "--package",
            "camber-macros",
        ],
        vec![
            "semver-checks",
            "check-release",
            "--package",
            "camber-build",
        ],
    ] {
        let (status, output) = check_adapter(&arguments, 0, true);
        assert_eq!(status, 0, "{output}");
        assert_eq!(&*output, format!("{}\n", arguments.join("\n")));
    }
}

#[test]
fn release_checker_requires_an_explicit_real_executable() {
    let (status, output) = check_adapter(&["--version"], 0, false);
    assert_eq!(status, 75, "{output}");
    assert!(output.contains("CAMBER_SEMVER_CHECKS_BIN"), "{output}");
}

#[test]
fn release_features_cover_every_non_allocator_capability() {
    let source = String::from_utf8(repository_file("crates/camber/Cargo.toml").into_vec())
        .expect("Cargo manifest is UTF-8");
    let manifest: toml::Value = toml::from_str(&source).expect("Cargo manifest is TOML");
    let features = manifest["features"].as_table().expect("feature table");
    let expected: BTreeSet<&str> = features
        .keys()
        .map(String::as_str)
        .filter(|feature| !matches!(*feature, "jemalloc" | "mimalloc"))
        .collect();
    assert_eq!(API_FEATURES.split(',').collect::<BTreeSet<_>>(), expected);
}

fn release_preflight_repo() -> FixtureRepo {
    let repo = FixtureRepo::new();
    for script in [
        ".github/scripts/reproduce-ci.sh",
        ".github/scripts/release.sh",
        ADAPTER,
    ] {
        repo.write_executable(script, repository_file(script));
    }
    repo.write_executable(
        "bin/cargo-semver-checks",
        b"#!/bin/bash\nprintf 'cargo-semver-checks 0.50.0\\n'\n",
    );
    repo.write_executable(
        "bin/release-plz",
        br#"#!/bin/bash
case "$*" in
    --version) printf 'release-plz 0.3.169\n' ;;
    update)
        printf 'release-preflight-ran\n'
        printf 'updated version\n' >release-marker
        exit "$PREFLIGHT_STATUS"
        ;;
    *) exit 99 ;;
esac
"#,
    );
    std::os::unix::fs::symlink(ambient_executable("git"), repo.path().join("bin/git"))
        .expect("fixture Git symlink");
    repo.commit("release preflight fixture");
    repo
}

#[test]
fn release_preflight_is_isolated_and_preserves_failure_status() {
    for status in [0, 42, 75] {
        let repo = release_preflight_repo();
        let mut command = repo.system_command(Path::new("/bin/bash"));
        command
            .args([".github/scripts/reproduce-ci.sh", "release"])
            .env(
                "PATH",
                format!("{}/bin:/usr/bin:/bin", repo.path().display()),
            )
            .env("PREFLIGHT_STATUS", status.to_string());
        let result = run_bounded(command);
        assert_eq!(result.status, status, "{}", result.output);
        assert!(result.output.contains("release-preflight-ran"));
        assert!(!repo.path().join("release-marker").exists());
        assert!(repo.git(&["status", "--porcelain"]).output.is_empty());
        assert_eq!(
            repo.git(&["worktree", "list", "--porcelain"])
                .output
                .matches("worktree ")
                .count(),
            1
        );
        repo.close();
    }
}
