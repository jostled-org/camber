use std::path::Path;

use crate::delivery_fixture::{
    FixtureRepo, HookRun, ambient_executable, repository_file, repository_root, run_bounded,
};

const REFRESH: &str = ".github/scripts/refresh-release-inputs.sh";
const RECORD: &str = ".github/supply-chain-inputs.sha256";

fn release_repo() -> FixtureRepo {
    let repo = FixtureRepo::new();
    repo.write("Cargo.lock", b"release lockfile\n");
    repo.write("Cargo.toml", b"[workspace]\n");
    repo.write(
        "crates/demo/Cargo.toml",
        b"[package]\nversion = \"0.2.0\"\n",
    );
    repo.write(
        "fixtures/probe/Cargo.toml",
        b"[package]\nversion = \"0.1.0\"\n",
    );
    repo.write(RECORD, b"stale release hashes\n");
    repo.write_executable(
        ".github/scripts/check-supply-chain.sh",
        repository_file(".github/scripts/check-supply-chain.sh"),
    );
    let script = repository_root().join(REFRESH);
    if script.exists() {
        repo.write_executable(REFRESH, repository_file(REFRESH));
    }
    install_github_fixture(&repo);
    for tool in ["git", "jq"] {
        std::os::unix::fs::symlink(ambient_executable(tool), repo.path().join("bin").join(tool))
            .unwrap();
    }
    repo.commit("generated release");
    repo.git(&["remote", "add", "origin", repo.path().to_str().unwrap()]);
    repo
}

fn install_github_fixture(repo: &FixtureRepo) {
    repo.write_executable(
        "bin/gh",
        br#"#!/bin/bash
set -eu
case "$*" in
    'api repos/test/camber/pulls/36') cat "$PR_RESPONSE" ;;
    'api graphql --input -')
        cat >"$MUTATION_RECORD"
        [ "${API_STATUS:-0}" = 0 ] || exit "$API_STATUS"
        printf '{"data":{"createCommitOnBranch":{"commit":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}}\n'
        ;;
    *) exit 99 ;;
esac
"#,
    );
}

fn run_refresh(
    repo: &FixtureRepo,
    output: &str,
    response: serde_json::Value,
    status: i32,
) -> HookRun {
    let output_path = repo.outside("release-pr.json");
    let response_path = repo.outside("response.json");
    std::fs::write(&output_path, output).unwrap();
    std::fs::write(&response_path, serde_json::to_vec(&response).unwrap()).unwrap();
    let mut command = repo.system_command(Path::new("/bin/bash"));
    command
        .arg(repo.path().join(REFRESH))
        .arg(output_path)
        .env("GITHUB_REPOSITORY", "test/camber")
        .env("PR_RESPONSE", response_path)
        .env("MUTATION_RECORD", repo.outside("mutation.json"))
        .env("API_STATUS", status.to_string())
        .env(
            "PATH",
            format!("{}/bin:/usr/bin:/bin", repo.path().display()),
        );
    run_bounded(command)
}

const RELEASE_OUTPUT: &str =
    r#"{"prs":[{"number":36,"head_branch":"release-plz-test","base_branch":"main"}]}"#;

fn pr_response(repo: &FixtureRepo) -> serde_json::Value {
    serde_json::json!({
        "state": "open",
        "head": {
            "ref": "release-plz-test",
            "sha": repo.git(&["rev-parse", "HEAD"]).output.trim(),
            "repo": {"full_name": "test/camber"}
        },
        "base": {"ref": "main", "repo": {"full_name": "test/camber"}}
    })
}

fn expected_record(repo: &FixtureRepo) -> String {
    let mut command = repo.system_command(Path::new("shasum"));
    command.args([
        "-a",
        "256",
        "--",
        "Cargo.lock",
        "Cargo.toml",
        "crates/demo/Cargo.toml",
        "fixtures/probe/Cargo.toml",
    ]);
    let run = run_bounded(command);
    assert_eq!(run.status, 0, "{}", run.output);
    run.output.into()
}

fn encoded_record(repo: &FixtureRepo, record: &str) -> String {
    let path = repo.outside("expected-record");
    std::fs::write(&path, record).unwrap();
    let mut command = repo.system_command(Path::new("/bin/bash"));
    command
        .args(["-c", "base64 < \"$1\" | tr -d '\\n'", "fixture"])
        .arg(path);
    let run = run_bounded(command);
    assert_eq!(run.status, 0, "{}", run.output);
    run.output.into()
}

#[test]
fn release_record_uses_the_release_commit_without_touching_the_checkout() {
    let repo = release_repo();
    let response = pr_response(&repo);
    let expected = encoded_record(&repo, &expected_record(&repo));
    repo.write("Cargo.lock", b"unrelated local dependency work\n");
    let before = repo.git(&["status", "--porcelain"]).output;
    let run = run_refresh(&repo, RELEASE_OUTPUT, response.clone(), 0);
    assert_eq!(
        run.status, 0,
        "release record refresh failed: {}",
        run.output
    );
    let mutation: serde_json::Value =
        serde_json::from_slice(&std::fs::read(repo.outside("mutation.json")).unwrap()).unwrap();
    let input = &mutation["variables"]["input"];
    assert_eq!(input["expectedHeadOid"], response["head"]["sha"]);
    assert_eq!(input["branch"]["branchName"], "release-plz-test");
    assert_eq!(input["branch"]["repositoryNameWithOwner"], "test/camber");
    assert_eq!(
        input["fileChanges"]["additions"].as_array().unwrap().len(),
        1
    );
    assert_eq!(input["fileChanges"]["additions"][0]["path"], RECORD);
    assert_eq!(input["fileChanges"]["additions"][0]["contents"], expected);
    assert!(input["fileChanges"].get("deletions").is_none());
    assert_eq!(repo.read("Cargo.lock"), "unrelated local dependency work\n");
    assert_eq!(repo.read(RECORD), "stale release hashes\n");
    assert_eq!(repo.git(&["status", "--porcelain"]).output, before);
    repo.close();
}

#[test]
fn release_record_skips_empty_output_and_an_already_current_record() {
    let repo = release_repo();
    let run = run_refresh(&repo, r#"{"prs":[]}"#, serde_json::Value::Null, 99);
    assert_eq!(run.status, 0, "{}", run.output);
    repo.write(RECORD, expected_record(&repo).as_bytes());
    repo.commit("record current release inputs");
    let run = run_refresh(&repo, RELEASE_OUTPUT, pr_response(&repo), 99);
    assert_eq!(run.status, 0, "{}", run.output);
    assert!(!repo.outside("mutation.json").exists());
    repo.close();
}

#[test]
fn release_record_refuses_untrusted_targets_and_preserves_api_failure() {
    let repo = release_repo();
    let response = pr_response(&repo);
    for (pointer, value) in [
        ("/state", "closed"),
        ("/head/ref", "main"),
        ("/head/repo/full_name", "foreign/camber"),
        ("/base/ref", "other"),
        ("/head/sha", "not-a-commit"),
    ] {
        let mut refused = response.clone();
        *refused.pointer_mut(pointer).unwrap() = value.into();
        let run = run_refresh(&repo, RELEASE_OUTPUT, refused, 0);
        assert_ne!(run.status, 0, "admitted {pointer}: {}", run.output);
        assert!(!repo.outside("mutation.json").exists());
    }
    let run = run_refresh(&repo, "not JSON", response.clone(), 0);
    assert_ne!(run.status, 0);
    assert!(!repo.outside("mutation.json").exists());
    let run = run_refresh(&repo, RELEASE_OUTPUT, response, 42);
    assert_eq!(run.status, 42, "API failure was erased: {}", run.output);
    repo.close();
}

#[test]
fn release_workflow_refreshes_the_generated_pr_record() {
    let workflow =
        String::from_utf8(repository_file(".github/workflows/release-plz.yml").into()).unwrap();
    assert!(workflow.contains("--output json > \"$RUNNER_TEMP/release-pr.json\""));
    assert!(workflow.contains(
        "bash .github/scripts/refresh-release-inputs.sh \"$RUNNER_TEMP/release-pr.json\""
    ));
}
