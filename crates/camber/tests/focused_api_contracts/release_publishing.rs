use std::path::Path;

use crate::delivery_fixture::{
    FixtureRepo, HookRun, ambient_executable, repository_file, repository_root, repository_text,
    run_bounded,
};

const PUBLISH: &str = ".github/scripts/publish-release.sh";
const SELECT: &str = ".github/scripts/select-release.sh";

fn release_repo() -> FixtureRepo {
    let repo = FixtureRepo::new();
    repo.write("Cargo.toml", b"[workspace]\n");
    repo.write("release-plz.toml", b"[workspace]\npublish = false\n");
    repo.write(
        "crates/camber/CHANGELOG.md",
        b"## [0.12.0] - today\n\nApp changes.\n\n## [0.11.0]\nOld changes.\n",
    );
    repo.write(
        "crates/camber-macros/CHANGELOG.md",
        b"## [0.7.0] - today\n\nMacro changes.\n",
    );
    for tool in ["cargo", "curl", "gh", "release-plz", "tar"] {
        repo.write_executable(
            &format!("bin/{tool}"),
            repository_file(&format!(
                "crates/camber/tests/fixtures/release_publishing/{tool}"
            )),
        );
    }
    repo.write_executable("bin/sleep", b"#!/bin/bash\nexit 0\n");
    std::os::unix::fs::symlink(ambient_executable("git"), repo.path().join("bin/git")).unwrap();
    repo.commit("release source");
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    repo
}

fn run_script(repo: &FixtureRepo, script: &str, values: &[(&str, &str)]) -> HookRun {
    let mut command = repo.system_command(Path::new("/bin/bash"));
    command
        .arg(repository_root().join(script))
        .env("GITHUB_REPOSITORY", "test/camber")
        .env("GITHUB_TOKEN", "fixture-token")
        .env("GITHUB_SHA", repo.git(&["rev-parse", "HEAD"]).output.trim())
        .env("GITHUB_OUTPUT", repo.outside("output"))
        .env("RELEASE_SOURCE", repo.path())
        .env("RELEASE_CONFIG", repo.path().join("release-plz.toml"))
        .env("FIXTURE_STATE", repo.outside("state"))
        .env("FIXTURE_LOG", repo.outside("calls"))
        .env(
            "FIXTURE_SHA",
            repo.git(&["rev-parse", "HEAD"]).output.trim(),
        )
        .env(
            "PATH",
            format!("{}/bin:{}", repo.path().display(), repo.utility_path()),
        )
        .envs(values.iter().copied());
    run_bounded(command)
}

fn calls(repo: &FixtureRepo) -> String {
    std::fs::read_to_string(repo.outside("calls")).unwrap_or_default()
}

#[test]
fn release_publish_accepts_warning_text_and_confirms_before_tagging() {
    let repo = release_repo();
    let run = run_script(&repo, PUBLISH, &[]);
    assert_eq!(run.status, 0, "{}", run.output);
    assert!(run.output.contains("error:0A000126"));
    let log = calls(&repo);
    assert!(log.contains("publish --locked --registry crates-io -p camber -p camber-macros"));
    assert!(log.find("publish ").unwrap() < log.find("release-plz release").unwrap());
    assert!(log.contains("confirm camber-macros"));
    repo.close();
}

#[test]
fn release_publish_resumes_partial_upload_without_republishing() {
    let repo = release_repo();
    let run = run_script(&repo, PUBLISH, &[("PUBLISHED_MACROS", "1")]);
    assert_eq!(run.status, 0, "{}", run.output);
    let log = calls(&repo);
    assert!(log.contains("publish --locked --registry crates-io -p camber\n"));
    assert!(!log.contains("-p camber-macros"));
    assert!(log.contains("release-plz release"));
    repo.close();
}

#[test]
fn release_publish_rejects_failures_without_creating_tags() {
    for (key, value, expected) in [
        ("PUBLISH_STATUS", "42", 42),
        ("REGISTRY_STATUS", "503", 1),
        ("CURL_STATUS", "35", 35),
        ("BAD_INDEX", "1", 1),
        ("NO_CONFIRMATION", "1", 1),
    ] {
        let repo = release_repo();
        let run = run_script(&repo, PUBLISH, &[(key, value)]);
        assert_eq!(run.status, expected, "{key}: {}", run.output);
        assert!(!calls(&repo).contains("release-plz"), "{key}");
        repo.close();
    }
}

#[test]
fn release_publish_checks_archive_identity_and_existing_tags() {
    for mismatch in ["archive", "tag"] {
        let repo = release_repo();
        let old = repo.git(&["rev-parse", "HEAD"]).output;
        repo.write("another", b"commit\n");
        repo.commit("next source");
        let mut values = vec![("PUBLISHED_MACROS", "1")];
        match mismatch {
            "archive" => values.push(("ARCHIVE_SHA", old.trim())),
            _ => {
                repo.git(&["tag", "camber-macros-v0.7.0", old.trim()]);
            }
        }
        let run = run_script(&repo, PUBLISH, &values);
        assert_ne!(run.status, 0, "{mismatch}: {}", run.output);
        assert!(!calls(&repo).contains("publish "));
        assert!(!calls(&repo).contains("release-plz"));
        repo.close();
    }
}

#[test]
fn release_publish_preserves_release_failure_and_skips_completed_uploads() {
    let repo = release_repo();
    let run = run_script(&repo, PUBLISH, &[("RELEASE_STATUS", "43")]);
    assert_eq!(run.status, 43, "{}", run.output);
    let run = run_script(&repo, PUBLISH, &[]);
    assert_eq!(run.status, 0, "{}", run.output);
    assert_eq!(calls(&repo).matches("publish --locked").count(), 1);
    repo.close();
}

#[test]
fn release_publish_recovers_a_tag_without_a_github_release() {
    let repo = release_repo();
    repo.git(&["tag", "camber-macros-v0.7.0"]);
    let values = [("PUBLISHED_MACROS", "1"), ("MISSING_RELEASE", "1")];
    let run = run_script(&repo, PUBLISH, &values);
    assert_eq!(run.status, 0, "{}", run.output);
    let log = calls(&repo);
    assert!(log.contains("release create camber-macros-v0.7.0 --verify-tag"));
    assert!(log.contains("Macro changes."));
    assert!(!log.contains("-p camber-macros"));
    repo.close();
}

fn pr_response(repo: &FixtureRepo) -> serde_json::Value {
    let sha = repo.git(&["rev-parse", "HEAD"]).output;
    serde_json::json!([{
        "merged_at": "2026-10-06T00:00:00Z",
        "merge_commit_sha": sha.trim(),
        "head": {"ref": "release-plz-test", "sha": sha.trim(),
            "repo": {"full_name": "test/camber"}},
        "base": {"ref": "main", "repo": {"full_name": "test/camber"}}
    }])
}

fn run_selection(repo: &FixtureRepo, response: &serde_json::Value) -> HookRun {
    let response = serde_json::to_string(response).unwrap();
    run_script(repo, SELECT, &[("PR_RESPONSE", &response)])
}

#[test]
fn release_selection_accepts_only_the_matching_merged_release_pr() {
    let repo = release_repo();
    let response = pr_response(&repo);
    let run = run_selection(&repo, &response);
    assert_eq!(run.status, 0, "{}", run.output);
    let output = std::fs::read_to_string(repo.outside("output")).unwrap();
    assert!(output.contains(repo.git(&["rev-parse", "HEAD"]).output.trim()));
    for (pointer, value) in [
        ("/0/merged_at", serde_json::Value::Null),
        ("/0/head/ref", "feature".into()),
        ("/0/head/repo/full_name", "foreign/camber".into()),
        ("/0/base/ref", "other".into()),
        ("/0/merge_commit_sha", "a".repeat(40).into()),
    ] {
        std::fs::remove_file(repo.outside("output")).unwrap();
        let mut refused = response.clone();
        *refused.pointer_mut(pointer).unwrap() = value;
        let run = run_selection(&repo, &refused);
        assert_eq!(run.status, 0, "{pointer}: {}", run.output);
        assert_eq!(
            std::fs::read_to_string(repo.outside("output")).unwrap(),
            "source=\n"
        );
    }
    repo.close();
}

#[test]
fn release_selection_preserves_api_errors_and_rejects_invalid_recovery() {
    let repo = release_repo();
    for (key, value) in [("GH_STATUS", "42"), ("RELEASE_COMMIT", "main")] {
        let run = run_script(&repo, SELECT, &[(key, value)]);
        assert_ne!(run.status, 0, "{}", run.output);
        assert!(!repo.outside("output").exists());
    }
    repo.close();
}

#[test]
fn release_selection_preserves_the_pr_source_and_supports_manual_recovery() {
    let repo = release_repo();
    let head = repo.git(&["rev-parse", "HEAD"]).output;
    repo.write("merge-marker", b"merge commit\n");
    repo.commit("merged release");
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let mut response = pr_response(&repo);
    response[0]["head"]["sha"] = head.trim().into();
    let response = serde_json::to_string(&response).unwrap();
    let merge = repo.git(&["rev-parse", "HEAD"]).output;
    let run = run_script(
        &repo,
        SELECT,
        &[("PR_RESPONSE", &response), ("RELEASE_COMMIT", merge.trim())],
    );
    assert_eq!(run.status, 0, "{}", run.output);
    assert_eq!(
        std::fs::read_to_string(repo.outside("output")).unwrap(),
        format!("source={}\n", head.trim())
    );
    let run = run_script(&repo, SELECT, &[("RELEASE_COMMIT", merge.trim())]);
    assert_ne!(run.status, 0, "{}", run.output);
    repo.close();
}

#[test]
fn release_selection_uses_the_merge_commit_when_the_pr_was_squashed() {
    let repo = release_repo();
    repo.git(&["checkout", "-b", "release-plz-test"]);
    repo.write("squashed", b"same content\n");
    repo.commit("release PR head");
    let head = repo.git(&["rev-parse", "HEAD"]).output;
    repo.git(&["checkout", "main"]);
    repo.write("squashed", b"same content\n");
    repo.commit("squash release");
    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let mut response = pr_response(&repo);
    response[0]["head"]["sha"] = head.trim().into();
    let run = run_selection(&repo, &response);
    assert_eq!(run.status, 0, "{}", run.output);
    assert_eq!(
        std::fs::read_to_string(repo.outside("output")).unwrap(),
        format!(
            "source={}\n",
            repo.git(&["rev-parse", "HEAD"]).output.trim()
        )
    );
    repo.close();
}

#[test]
fn release_workflow_separates_publishing_and_uses_pinned_tools() {
    let workflow = repository_text(".github/workflows/release-plz.yml");
    assert!(workflow.contains("workflow_dispatch:"));
    assert!(workflow.contains("select-release.sh"));
    assert!(workflow.contains("publish-release.sh"));
    assert!(!workflow.contains("rust-toolchain@stable"));
    assert!(!workflow.contains("MarcoIeni/release-plz-action"));
    let config: toml::Value = toml::from_str(&repository_text("release-plz.toml")).unwrap();
    assert_eq!(config["workspace"]["publish"].as_bool(), Some(false));
    assert_ne!(
        config["workspace"]
            .get("git_only")
            .and_then(toml::Value::as_bool),
        Some(true)
    );
}
