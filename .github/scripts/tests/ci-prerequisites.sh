#!/usr/bin/env bash

set -euo pipefail

ROOT=$(CDPATH='' cd "${BASH_SOURCE[0]%/*}/../../.." && pwd)
SELFTEST="${ROOT}/.github/scripts/ci-selftest.sh"
REPRODUCE="${ROOT}/.github/scripts/reproduce-ci.sh"
BASH_EXE="${BASH}"
FIXTURE=$(mktemp -d "${TMPDIR:-/tmp}/camber-ci-prerequisites.XXXXXX")
trap 'rm -rf -- "${FIXTURE}"' EXIT
mkdir "${FIXTURE}/bin"
ln -s "$(command -v dirname)" "${FIXTURE}/bin/dirname"

expect_exit() {
    local expected="$1" label="$2" actual=0
    shift 2
    OUTPUT=$("$@" 2>&1) || actual=$?
    if [ "${actual}" -ne "${expected}" ]; then
        printf '%s: expected exit %s, received %s\n%s\n' \
            "${label}" "${expected}" "${actual}" "${OUTPUT}" >&2
        exit 1
    fi
}

expect_message() {
    case "${OUTPUT}" in
        *"$1"*) return 0 ;;
        *) printf 'missing diagnostic: %s\n%s\n' "$1" "${OUTPUT}" >&2; exit 1 ;;
    esac
}

reject_message() {
    case "${OUTPUT}" in
        *"$1"*) printf 'misleading diagnostic: %s\n%s\n' "$1" "${OUTPUT}" >&2; exit 1 ;;
        *) return 0 ;;
    esac
}

expect_exit 75 'self-test without ripgrep' \
    env PATH="${FIXTURE}/bin" "${BASH_EXE}" "${SELFTEST}"
expect_message 'required hook tool is unavailable: rg'
reject_message 'CI omits hook self-test'

expect_exit 75 'workflow reproduction without ripgrep' "${BASH_EXE}" -s -- \
    "${REPRODUCE}" "${FIXTURE}/bin" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    git() { :; }
    cargo() { :; }
    cargo-deny() { :; }
    pedant() { :; }
    protoc() { :; }
    PATH="$2"
    require_workflow_tools
BASH
expect_message 'required workflow tool is unavailable: rg'

expect_exit 0 'present workflow entry' "${BASH_EXE}" -s -- "${SELFTEST}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    assert_workflow_entry ".github/scripts/ci-selftest.sh" "hook self-test"
BASH

expect_exit 1 'absent workflow entry' "${BASH_EXE}" -s -- "${SELFTEST}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    assert_workflow_entry "camber-deliberately-absent-workflow-entry" "fixture entry"
BASH
expect_message 'CI omits fixture entry'

expect_exit 2 'workflow search error' "${BASH_EXE}" -s -- "${SELFTEST}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    rg() { return 2; }
    assert_workflow_entry ".github/scripts/ci-selftest.sh" "hook self-test"
BASH
expect_message 'workflow search failed (rg exit 2)'
reject_message 'CI omits hook self-test'

mkdir -p "${FIXTURE}/workflow/.github/scripts"
for hook in ci-selftest check-pedant check-supply-chain; do
    ln -s /usr/bin/true "${FIXTURE}/workflow/.github/scripts/${hook}.sh"
done

for doc_status in 0 42 75; do
    expect_exit "${doc_status}" "workflow documentation status ${doc_status}" \
        "${BASH_EXE}" -s -- "${REPRODUCE}" "${FIXTURE}/workflow" "${doc_status}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    DOC_STATUS="$3"
    DOC_SEEN=0
    cargo() {
        case "$*" in
            '--config build.rustdocflags=["-D","warnings"] doc --workspace --lib --features profiling,ws,grpc,acme,dns01,nats,sqs,otel --no-deps')
                DOC_SEEN=1
                printf 'documentation command observed\n'
                return "${DOC_STATUS}"
                ;;
            'deny --workspace check') printf 'post-documentation phase observed\n' ;;
        esac
        return 0
    }
    status=0
    run_workflow_checks "$2" "$2" || status=$?
    [ "${DOC_SEEN}" = 1 ] || exit 98
    exit "${status}"
BASH
    expect_message 'documentation command observed'
    case "${doc_status}" in
        0) expect_message 'post-documentation phase observed' ;;
        *) reject_message 'post-documentation phase observed' ;;
    esac
done

expect_exit 1 'changed manifests after reviewed dependency base' \
    "${BASH_EXE}" -s -- "${ROOT}/.github/scripts/check-supply-chain.sh" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    CAMBER_DEPENDENCY_BASE_SHA=reviewed-base
    git() { printf 'dependency diff: %s\n' "$*"; return 1; }
    validate_dependency_inputs
BASH
expect_message 'dependency diff: diff --quiet reviewed-base..HEAD'

printf 'CI prerequisite regression tests: PASS\n'
