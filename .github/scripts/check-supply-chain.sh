#!/usr/bin/env bash

set -euo pipefail

readonly DEPENDENCY_INPUT_RECORD='.github/supply-chain-inputs.sha256'
readonly RUSTLS_WEBPKI_EXCEPTION='error: rustls-webpki: target webpki has no complete source closure: [src/lib.rs declares mod test_utils] src/lib.rs declares mod test_utils: no source exists for the declared module (attempted src/test_utils.rs)'
# The lockfile and every tracked Cargo manifest, fixtures included.
DEPENDENCY_INPUT_PATHSPECS=(Cargo.lock Cargo.toml ':(glob)**/Cargo.toml')

dependency_input_paths() {
    git -c core.quotepath=off ls-files -- "${DEPENDENCY_INPUT_PATHSPECS[@]}" \
        | LC_ALL=C sort
}

# The exact record of the current checkout. Renew the tracked record from this
# output only as part of a reviewed dependency change.
render_dependency_input_record() {
    local paths input
    local -a inputs=()
    paths=$(dependency_input_paths) || return 75
    [ -n "${paths}" ] || {
        printf 'ERROR: no tracked dependency inputs\n' >&2
        return 1
    }
    while IFS= read -r input; do
        inputs+=("${input}")
    done <<<"${paths}"
    command -v shasum >/dev/null 2>&1 || {
        printf 'INFRASTRUCTURE: required supply-chain tool is unavailable: shasum\n' >&2
        return 75
    }
    shasum -a 256 -- "${inputs[@]}"
}

# Admit the checkout only when its record states every input byte for byte.
validate_dependency_inputs() {
    local expected status=0
    [ -f "${DEPENDENCY_INPUT_RECORD}" ] || {
        printf 'ERROR: reviewed dependency input record is missing: %s\n' \
            "${DEPENDENCY_INPUT_RECORD}" >&2
        return 1
    }
    expected=$(render_dependency_input_record) || return $?
    printf '%s\n' "${expected}" \
        | diff -u --label "${DEPENDENCY_INPUT_RECORD}" --label 'current checkout' \
            -- "${DEPENDENCY_INPUT_RECORD}" - >&2 \
        || status=$?
    case "${status}" in
        0) return 0 ;;
        1)
            printf 'ERROR: dependency inputs differ from %s; review the change and renew the record\n' \
                "${DEPENDENCY_INPUT_RECORD}" >&2
            return 1
            ;;
        126|127) return 75 ;;
        *) return "${status}" ;;
    esac
}

# Name the recorded inputs that changed since the caller's reviewed base.
report_reviewed_input_changes() {
    local base="${CAMBER_DEPENDENCY_BASE_SHA:-}" changes
    [ -n "${base}" ] || return 0
    git rev-parse --verify --quiet "${base}^{commit}" >/dev/null || {
        printf 'ERROR: reviewed dependency base is not a commit: %s\n' "${base}" >&2
        return 1
    }
    changes=$(git diff --name-only "${base}" HEAD -- "${DEPENDENCY_INPUT_PATHSPECS[@]}") \
        || return 75
    case "${changes}" in
        '') printf 'Dependency inputs are unchanged since %s\n' "${base}" ;;
        *) printf 'Recorded dependency inputs changed since %s:\n%s\n' "${base}" "${changes}" ;;
    esac
}

accept_reviewed_exception() {
    [ "$1" = "${RUSTLS_WEBPKI_EXCEPTION}" ] || return 2
    printf 'NOTICE: policy accepted an incomplete supply-chain verification run: the reviewed rustls-webpki source-closure exception stopped Pedant before baseline comparison\n'
}

verify_supply_chain() {
    local output status=0
    output=$(pedant supply-chain verify 2>&1) || status=$?
    printf '%s\n' "${output}"
    case "${status}" in
        0) return 0 ;;
        2) accept_reviewed_exception "${output}" ;;
        126|127) return 75 ;;
        *) return "${status}" ;;
    esac
}

check_supply_chain_main() {
    local root
    root=$(git rev-parse --show-toplevel 2>/dev/null) || return 75
    cd "${root}" || return 75
    command -v pedant >/dev/null 2>&1 || {
        printf 'INFRASTRUCTURE: required supply-chain tool is unavailable: pedant\n' >&2
        return 75
    }
    validate_dependency_inputs || return $?
    report_reviewed_input_changes || return $?
    verify_supply_chain
}

[ "${CAMBER_HOOK_LIBRARY_MODE:-0}" = 1 ] || check_supply_chain_main "$@"
