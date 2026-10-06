#!/usr/bin/env bash
set -euo pipefail

cd "${RELEASE_SOURCE:?missing selected release checkout}"
: "${RELEASE_CONFIG:?missing release configuration}"
: "${GITHUB_REPOSITORY:?missing repository}"
: "${GITHUB_TOKEN:?missing GitHub token}"
source_commit=$(git rev-parse HEAD)
[ -z "$(git status --porcelain)" ] || {
    printf 'ERROR: release source must be clean\n' >&2
    exit 1
}
release_temp=$(mktemp -d "${TMPDIR:-/tmp}/camber-publish.XXXXXX")
trap 'rm -rf -- "${release_temp}"' EXIT

registry_state() {
    local name="$1" version="$2" index_path status
    case ${#name} in
        1|2) index_path="${#name}/${name}" ;;
        3) index_path="3/${name:0:1}/${name}" ;;
        *) index_path="${name:0:2}/${name:2:2}/${name}" ;;
    esac
    status=$(curl --silent --show-error --location --retry 3 --max-time 60 \
        --output "${release_temp}/index" --write-out '%{http_code}' \
        "https://index.crates.io/${index_path}") || return $?
    case ${status} in
        404) printf 'absent\n' ;;
        200)
            jq -ers --arg name "${name}" --arg version "${version}" '
                if length == 0 or any(.[]; .name != $name
                    or (.vers | type) != "string" or (.yanked | type) != "boolean")
                then error("invalid registry index") else . end
                | [.[] | select(.vers == $version)]
                | if length == 0 then "absent"
                  elif length != 1 or .[0].yanked then error("unusable registry version")
                  else "present" end
            ' "${release_temp}/index" || return 1
            ;;
        *) printf 'ERROR: registry query for %s returned HTTP %s\n' "${name}" "${status}" >&2; return 1 ;;
    esac
}

archive_commit() {
    local name="$1" version="$2"
    curl --fail --silent --show-error --location --retry 3 --retry-all-errors --max-time 120 \
        --output "${release_temp}/crate" \
        "https://static.crates.io/crates/${name}/${name}-${version}.crate" || return $?
    tar -xzOf "${release_temp}/crate" "${name}-${version}/.cargo_vcs_info.json" \
        | jq -er '.git.sha1 | select(test("^[0-9a-f]{40}$"))'
}

confirm_package() {
    local name="$1" version="$2" attempt state
    for attempt in {1..12}; do
        state=$(registry_state "${name}" "${version}") || return $?
        if [ "${state}" = present ]; then
            check_published_source "${name}" "${version}" "${name}-v${version}"
            return $?
        fi
        [ "${attempt}" = 12 ] || sleep 5
    done
    printf 'ERROR: registry has not confirmed %s %s; retry this release\n' "${name}" "${version}" >&2
    return 1
}

# Existing releases may come from older commits. A missing tag must match this source.
check_published_source() {
    local name="$1" version="$2" tag="$3" published tagged
    published=$(archive_commit "${name}" "${version}") || return $?
    if git show-ref --verify --quiet "refs/tags/${tag}"; then
        tagged=$(git rev-parse "refs/tags/${tag}^{commit}") || return $?
        [ "${tagged}" = "${published}" ] && return 0
        printf 'ERROR: tag %s does not match the published crate source\n' "${tag}" >&2
        return 1
    fi
    [ "${published}" = "${source_commit}" ] && return 0
    printf 'ERROR: missing tag %s requires recovery at published source %s\n' "${tag}" "${published}" >&2
    return 1
}

# release-plz skips existing tags, even when GitHub release creation failed.
recover_github_release() {
    local name="$1" version="$2" tag="$3" manifest
    if jq -e --arg tag "${tag}" 'any(.[]; .tag_name == $tag)' "${release_temp}/releases" >/dev/null; then
        return 0
    fi
    manifest=$(jq -er --arg name "${name}" '.packages[] | select(.name == $name) | .manifest_path' \
        "${release_temp}/metadata") || return $?
    jq -eRs --arg version "${version}" '
        ("\n" + .) | split("\n## [") | map(select(startswith($version + "]")))
        | if length != 1 then error("release changelog section is missing") else .[0] end
        | split("\n") | .[1:] | join("\n")
    ' "${manifest%/*}/CHANGELOG.md" >"${release_temp}/notes" || return $?
    gh release create "${tag}" --verify-tag --repo "${GITHUB_REPOSITORY}" \
        --title "${tag}" --latest=false --notes-file "${release_temp}/notes"
}

cargo metadata --format-version 1 --no-deps --locked >"${release_temp}/metadata"
packages=$(jq -er '
    .workspace_members as $members
    | [.packages[] | select(.id as $id | $members | index($id))
        | select(.publish == null or (.publish | length) > 0)
        | if .publish != null and .publish != ["crates-io"]
          then error("only crates.io publishing is supported") else . end
        | [.name, .version] | @tsv]
    | if length == 0 then error("no publishable workspace packages") else .[] end
' "${release_temp}/metadata")
pending=()
while IFS=$'\t' read -r name version; do
    tag="${name}-v${version}"
    state=$(registry_state "${name}" "${version}")
    case ${state} in
        present) check_published_source "${name}" "${version}" "${tag}" ;;
        absent)
            if git show-ref --verify --quiet "refs/tags/${tag}"; then
                printf 'ERROR: tag %s exists but its crate version is absent\n' "${tag}" >&2
                exit 1
            fi
            pending+=(-p "${name}")
            ;;
    esac
done <<<"${packages}"

# Cargo orders workspace packages by dependency. Warning text never determines success.
if [ "${#pending[@]}" -gt 0 ]; then
    cargo publish --locked --registry crates-io "${pending[@]}"
fi
while IFS=$'\t' read -r name version; do
    confirm_package "${name}" "${version}"
done <<<"${packages}"

release-plz release --config "${RELEASE_CONFIG}" --git-token "${GITHUB_TOKEN}"
gh api --paginate --slurp "repos/${GITHUB_REPOSITORY}/releases?per_page=100" \
    | jq -e 'add | if type == "array" then . else error("invalid release list") end' \
    >"${release_temp}/releases"
while IFS=$'\t' read -r name version; do
    git rev-parse --verify "refs/tags/${name}-v${version}^{commit}" >/dev/null
    recover_github_release "${name}" "${version}" "${name}-v${version}"
    gh api "repos/${GITHUB_REPOSITORY}/releases/tags/${name}-v${version}" >/dev/null
done <<<"${packages}"
