#!/usr/bin/env bash
set -euo pipefail

ROOT=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CAMBER_HOOK_LIBRARY_MODE=1 source "${ROOT}/.github/scripts/check-supply-chain.sh"

release_input_paths() {
    git -c core.quotepath=off ls-tree -r --name-only "$1" \
        | while IFS= read -r path; do
            case "${path}" in
                Cargo.lock|Cargo.toml|*/Cargo.toml) printf '%s\n' "${path}" ;;
            esac
        done | LC_ALL=C sort
}

# Read committed blobs without checking out or executing release-branch files.
render_release_input_record() {
    local head="$1" paths path digest
    paths=$(release_input_paths "${head}") || return $?
    [ -n "${paths}" ] || return 1
    while IFS= read -r path; do
        digest=$(git show "${head}:${path}" | shasum -a 256) || return $?
        printf '%s  %s\n' "${digest%% *}" "${path}"
    done <<<"${paths}"
}

release_pr_head() {
    local number="$1" branch="$2" base="$3" response
    response=$(gh api "repos/${GITHUB_REPOSITORY}/pulls/${number}") || return $?
    jq -er --arg repo "${GITHUB_REPOSITORY}" --arg branch "${branch}" --arg base "${base}" '
        select(.state == "open"
            and .head.repo.full_name == $repo
            and .base.repo.full_name == $repo
            and .head.ref == $branch
            and .base.ref == $base)
        | .head.sha | select(test("^[0-9a-f]{40}$"))
    ' <<<"${response}"
}

# Compare-and-swap prevents recording hashes for a branch that moved meanwhile.
publish_release_input_record() {
    local branch="$1" head="$2" record="$3" encoded
    encoded=$(printf '%s\n' "${record}" | base64 | tr -d '\n') || return $?
    jq -n --arg repo "${GITHUB_REPOSITORY}" --arg branch "${branch}" \
        --arg head "${head}" --arg path "${DEPENDENCY_INPUT_RECORD}" --arg encoded "${encoded}" '
        {
            query: "mutation($input: CreateCommitOnBranchInput!) { createCommitOnBranch(input: $input) { commit { oid } } }",
            variables: {input: {
                branch: {repositoryNameWithOwner: $repo, branchName: $branch},
                expectedHeadOid: $head,
                message: {headline: "chore: refresh release dependency inputs"},
                fileChanges: {additions: [{path: $path, contents: $encoded}]}
            }}
        }
    ' | gh api graphql --input -
}

refresh_release_pr() {
    local number="$1" branch="$2" base="$3" head record recorded expected
    head=$(release_pr_head "${number}" "${branch}" "${base}") || {
        printf 'ERROR: release PR does not match the expected open branch and repository\n' >&2
        return 1
    }
    git fetch --no-tags origin "${head}" || return $?
    record=$(render_release_input_record "${head}") || return $?
    recorded=$(git rev-parse --verify "${head}:${DEPENDENCY_INPUT_RECORD}") || return $?
    expected=$(printf '%s\n' "${record}" | git hash-object --stdin) || return $?
    [ "${recorded}" != "${expected}" ] || {
        printf 'Release PR #%s already records its dependency inputs\n' "${number}"
        return 0
    }
    publish_release_input_record "${branch}" "${head}" "${record}"
}

refresh_release_inputs_main() {
    local output="${1:?expected release-plz JSON output file}" prs number branch base
    cd "${ROOT}"
    [[ ${GITHUB_REPOSITORY:-} =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || {
        printf 'ERROR: GITHUB_REPOSITORY must identify the release repository\n' >&2
        return 1
    }
    jq -e '
        type == "object" and (.prs | type == "array")
        and all(.prs[];
            (.number | type == "number" and . > 0 and floor == .)
            and (.head_branch | type == "string" and startswith("release-plz-"))
            and (.base_branch | type == "string" and length > 0)
            and .head_branch != .base_branch)
    ' "${output}" >/dev/null || {
        printf 'ERROR: release-plz returned malformed release PR output\n' >&2
        return 1
    }
    prs=$(jq -r '.prs[] | [.number, .head_branch, .base_branch] | @tsv' "${output}") || return $?
    [ -n "${prs}" ] || return 0
    while IFS=$'\t' read -r number branch base; do
        refresh_release_pr "${number}" "${branch}" "${base}" || return $?
    done <<<"${prs}"
}

refresh_release_inputs_main "$@"
