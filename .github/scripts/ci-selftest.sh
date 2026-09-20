#!/usr/bin/env bash

set -euo pipefail

ROOT=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

fail() {
    printf 'CI self-test failed: %s\n' "$1" >&2
    exit 1
}

require_executable() {
    local path="$1"
    [ -x "${ROOT}/${path}" ] || fail "not executable: ${path}"
}

assert_output() {
    local expected="$1" actual="$2" label="$3"
    [ "${actual}" = "${expected}" ] || fail "${label}"
}

assert_workflow_entry() {
    local pattern="$1" label="$2" status=0
    rg -qF -- "${pattern}" "${ROOT}/.github/workflows/ci.yml" || status=$?
    case "${status}" in
        0) return 0 ;;
        1) fail "CI omits ${label}" ;;
        *)
            printf 'CI self-test: workflow search failed (rg exit %s)\n' \
                "${status}" >&2
            return "${status}"
            ;;
    esac
}

check_hook_inventories() {
    local script
    for script in \
        .github/scripts/check-pedant.sh \
        .github/scripts/check-supply-chain.sh \
        .github/scripts/reproduce-ci.sh \
        .github/scripts/verify-packages.sh; do
        require_executable "${script}"
    done

    CAMBER_HOOK_LIBRARY_MODE=1 source "${ROOT}/.github/scripts/check-pedant.sh"
    assert_output \
        $'camber\ncamber-bench\ncamber-build\ncamber-cli\ncamber-macros' \
        "$(camber_pedant_packages)" \
        'Pedant package inventory drifted'

    CAMBER_HOOK_LIBRARY_MODE=1 source "${ROOT}/.github/scripts/verify-packages.sh"
    assert_output \
        $'camber-build\ncamber-macros\ncamber\ncamber-cli' \
        "$(camber_publishable_packages)" \
        'publishable package inventory drifted'

    CAMBER_HOOK_LIBRARY_MODE=1 source "${ROOT}/.github/scripts/reproduce-ci.sh"
    assert_output \
        $'hook-contract\nfmt\nclippy\ndoc\ntest\ndeny\npedant-source\npedant-tests\nsupply-chain' \
        "$(camber_workflow_checks)" \
        'workflow check inventory drifted'
}

check_workflow_entries() {
    assert_workflow_entry 'cargo --config '\''build.rustdocflags=["-D","warnings"]'\'' doc' \
        'warning-denying API documentation check' || return $?
    assert_workflow_entry "'**/*.md'" 'Markdown change trigger' || return $?
    assert_workflow_entry '.github/scripts/ci-selftest.sh' \
        'hook self-test' || return $?
    assert_workflow_entry '.github/scripts/check-pedant.sh source' \
        'shared source Pedant hook' || return $?
    assert_workflow_entry '.github/scripts/check-pedant.sh tests' \
        'shared test Pedant hook' || return $?
    assert_workflow_entry '.github/scripts/check-supply-chain.sh' \
        'shared supply-chain hook' || return $?
}

ci_selftest_main() {
    command -v rg >/dev/null 2>&1 || {
        printf 'INFRASTRUCTURE: required hook tool is unavailable: rg\n' >&2
        return 75
    }
    check_hook_inventories || return $?
    check_workflow_entries || return $?
    bash "${ROOT}/.github/scripts/tests/ci-prerequisites.sh" || return $?
    printf 'CI self-test: PASS\n'
}

[ "${CAMBER_HOOK_LIBRARY_MODE:-0}" = 1 ] || ci_selftest_main "$@"
