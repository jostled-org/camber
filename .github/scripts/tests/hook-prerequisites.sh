#!/usr/bin/env bash

set -euo pipefail

ROOT=$(CDPATH='' cd "${BASH_SOURCE[0]%/*}/../../.." && pwd)
SELFTEST="${ROOT}/.github/scripts/plan-loop-hooks-selftest.sh"
REPRODUCE="${ROOT}/.github/scripts/reproduce-ci.sh"
BASH_EXE="${BASH}"
FIXTURE=$(mktemp -d "${TMPDIR:-/tmp}/camber-hook-prerequisites.XXXXXX")
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
    assert_workflow_entry ".github/scripts/plan-loop-hooks-selftest.sh" "hook self-test"
BASH

expect_exit 1 'absent workflow entry' "${BASH_EXE}" -s -- "${SELFTEST}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    assert_workflow_entry "camber-deliberately-absent-workflow-entry" "fixture entry"
BASH
expect_message 'CI omits fixture entry'

expect_exit 2 'workflow search error' "${BASH_EXE}" -s -- "${SELFTEST}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    rg() { return 2; }
    assert_workflow_entry ".github/scripts/plan-loop-hooks-selftest.sh" "hook self-test"
BASH
expect_message 'workflow search failed (rg exit 2)'
reject_message 'CI omits hook self-test'

printf 'Hook prerequisite regression tests: PASS\n'
