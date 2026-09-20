#!/usr/bin/env bash

set -euo pipefail

ROOT=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

fail() {
    printf 'plan-loop hook self-test failed: %s\n' "$1" >&2
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
    $'hook-contract\nfmt\nclippy\ntest\ndeny\npedant-source\npedant-tests\nsupply-chain' \
    "$(camber_workflow_checks)" \
    'workflow check inventory drifted'

rg -qF -- ".github/scripts/plan-loop-hooks-selftest.sh" \
    "${ROOT}/.github/workflows/ci.yml" || fail 'CI omits hook self-test'
rg -qF -- ".github/scripts/check-pedant.sh source" \
    "${ROOT}/.github/workflows/ci.yml" || fail 'CI omits shared source Pedant hook'
rg -qF -- ".github/scripts/check-pedant.sh tests" \
    "${ROOT}/.github/workflows/ci.yml" || fail 'CI omits shared test Pedant hook'
rg -qF -- ".github/scripts/check-supply-chain.sh" \
    "${ROOT}/.github/workflows/ci.yml" || fail 'CI omits shared supply-chain hook'

printf 'plan-loop hook self-test: PASS\n'
