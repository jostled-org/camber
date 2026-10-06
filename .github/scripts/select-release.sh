#!/usr/bin/env bash
set -euo pipefail

# Only a merged release PR can select publishable source. Recovery uses its merge SHA.
commit=${RELEASE_COMMIT:-${GITHUB_SHA:?missing workflow commit}}
[[ ${commit} =~ ^[0-9a-f]{40}$ ]] || {
    printf 'ERROR: release commit must be a full commit SHA\n' >&2
    exit 1
}
git merge-base --is-ancestor "${commit}" origin/main || {
    printf 'ERROR: release commit is not in origin/main\n' >&2
    exit 1
}
response=$(gh api "repos/${GITHUB_REPOSITORY:?missing repository}/commits/${commit}/pulls")
heads=$(jq -er --arg repo "${GITHUB_REPOSITORY}" --arg commit "${commit}" '
    if type != "array" then error("expected pull request array") else . end
    | [.[] | select(.merged_at != null and .merge_commit_sha == $commit
        and .base.ref == "main" and .base.repo.full_name == $repo
        and .head.repo.full_name == $repo and (.head.ref | startswith("release-plz-")))
        | .head.sha]
    | if length > 1 then error("multiple release PRs match") else . end
    | if length == 0 then "" else .[0] end
' <<<"${response}")
if [ -z "${heads}" ]; then
    [ -z "${RELEASE_COMMIT:-}" ] || {
        printf 'ERROR: recovery commit is not a merged release PR\n' >&2
        exit 1
    }
    printf 'source=\n' >>"${GITHUB_OUTPUT:?missing output file}"
    exit 0
fi
[[ ${heads} =~ ^[0-9a-f]{40}$ ]] || {
    printf 'ERROR: release PR returned an invalid source SHA\n' >&2
    exit 1
}
# Match release-plz: use the PR head for merge commits, the merge SHA for squash merges.
source_commit=${commit}
if git merge-base --is-ancestor "${heads}" "${commit}"; then
    source_commit=${heads}
fi
printf 'source=%s\n' "${source_commit}" >>"${GITHUB_OUTPUT:?missing output file}"
