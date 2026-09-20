#!/usr/bin/env bash

set -euo pipefail

readonly EXPECTED_LOCK_SHA256='0c4afa217ab6eac2b615613fb32c81ffc7b1b09135d8fa3e2a8263b9dcf9c0cd'
readonly RUSTLS_WEBPKI_EXCEPTION='error: rustls-webpki: target webpki has no complete source closure: [src/lib.rs declares mod test_utils] src/lib.rs declares mod test_utils: no source exists for the declared module (attempted src/test_utils.rs)'

validate_dependency_inputs() {
    local lock_sha
    lock_sha=$(shasum -a 256 Cargo.lock | cut -d ' ' -f 1) || return 75
    [ "${lock_sha}" = "${EXPECTED_LOCK_SHA256}" ] || {
        printf 'ERROR: Cargo.lock changed without a reviewed supply-chain baseline\n' >&2
        return 1
    }
    [ -z "${PLAN_BASE_SHA:-}" ] \
        || git diff --quiet "${PLAN_BASE_SHA}"..HEAD -- \
            Cargo.toml ':(glob)**/Cargo.toml' \
        || {
            printf 'ERROR: Cargo manifests changed during the plan\n' >&2
            return 1
        }
}

verify_supply_chain() {
    local output status=0
    output=$(pedant supply-chain verify 2>&1) || status=$?
    printf '%s\n' "${output}"
    case "${status}" in
        0) return 0 ;;
        2) [ "${output}" = "${RUSTLS_WEBPKI_EXCEPTION}" ] ;;
        126|127) return 75 ;;
        *) return "${status}" ;;
    esac
}

check_supply_chain_main() {
    local root
    root=$(git rev-parse --show-toplevel 2>/dev/null) || return 75
    cd "${root}" || return 75
    command -v pedant >/dev/null 2>&1 || return 75
    validate_dependency_inputs
    verify_supply_chain
}

[ "${CAMBER_HOOK_LIBRARY_MODE:-0}" = 1 ] || check_supply_chain_main "$@"
