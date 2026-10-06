# Releases

Release-plz prepares version and changelog updates. After its PR merges, Cargo publishes the missing crate versions in dependency order.
Release-plz then creates tags and GitHub releases. Camber disables release-plz's Cargo invocation with `publish = false` in `release-plz.toml`.
This does not disable publishing in the crate manifests or change registry-based version discovery.

The workflow uses the release source's pinned Rust toolchain and release-plz 0.3.169.
It selects the merged release PR's source commit, not the latest code on `main`.
Normal code pushes prepare release PRs but do not publish crates.

Cargo's exit status determines whether publication succeeded. Recovered network warnings that contain `error:` do not cause a false failure.
The workflow confirms every version in the registry before creating tags.
Registry errors, malformed responses, yanked versions, and genuine Cargo failures stop the workflow.

## Recover an interrupted release

1. Make sure the updated workflow is on `main`.
2. Find the full merge commit SHA of the interrupted release PR.
3. Open **Actions → Release → Run workflow** on `main`.
4. Set `release_commit` to that SHA and start the workflow.

Use the merge SHA, not the PR head SHA. The workflow selects the original source from the merged PR.
For a squash merge, it uses the merge commit itself.

Recovery skips uploaded versions and checks their source against `.cargo_vcs_info.json` in the published archives.
It creates missing tags only at that source. Conflicting tags cause a failure; the workflow never moves them.
If a tag exists without a GitHub release, recovery creates the release from the matching changelog section.
No version bump or repeat upload is needed.

Rerunning an old failed Actions run uses its old workflow. Use **Run workflow** to recover with the updated implementation.

## Local checks

Run the release contract tests without contacting crates.io or publishing anything:

```sh
cargo test -p camber --test focused_api_contracts release_
shellcheck .github/scripts/select-release.sh .github/scripts/publish-release.sh
```

The tests use real Git repositories and controlled Cargo, registry, and GitHub responses.
They cover source selection, warning handling, failed uploads, partial publication, and missing release metadata.
