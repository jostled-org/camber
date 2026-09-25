#!/usr/bin/env bash

set -euo pipefail

readonly RUST_TOOLCHAIN_RECORD='rust-toolchain.toml'
readonly WORKFLOW_TOOL_RECORD='.github/workflow-tools.toml'
# The feature set every Cargo phase builds. CI states it as CAMBER_CI_FEATURES.
readonly CAMBER_WORKFLOW_FEATURES='profiling,ws,grpc,acme,dns01,nats,sqs,otel'
# The compiler flags every Cargo phase builds with. CI states them as RUSTFLAGS.
readonly CAMBER_WORKFLOW_RUSTFLAGS='-D warnings'

# A rustup proxy installs a pinned toolchain or component it lacks on first
# use. Verification admits installed tools; it never installs them.
export RUSTUP_AUTO_INSTALL=0

camber_workflow_checks() {
    printf '%s\n' \
        hook-contract fmt clippy doc test deny pedant-source pedant-tests supply-chain
}

# Workflow tools admitted by presence alone.
camber_present_workflow_tools() {
    printf '%s\n' git rg
}

# Workflow tools admitted only by the identity their records pin.
camber_pinned_workflow_tools() {
    printf '%s\n' rustc cargo clippy rustfmt cargo-deny pedant
}

# Pinned workflow tools CI builds with `cargo install`, not rustup.
camber_installed_workflow_tools() {
    printf '%s\n' cargo-deny pedant
}

# Every tool a full workflow run admits, in the order it is checked.
camber_workflow_tools() {
    camber_present_workflow_tools
    camber_pinned_workflow_tools
}

# Succeed when the inventory function `producer` lists `expected`.
listed() {
    local expected="$1" producer="$2" item
    while IFS= read -r item; do
        [ "${item}" = "${expected}" ] && return 0
    done < <("${producer}")
    return 1
}

# The executable Cargo or the workflow resolves for `tool`.
workflow_tool_executable() {
    case "$1" in
        clippy) printf 'cargo-clippy\n' ;;
        rustfmt) printf 'cargo-fmt\n' ;;
        *) printf '%s\n' "$1" ;;
    esac
}

workflow_tool_version() {
    case "$1" in
        clippy) cargo clippy --version ;;
        rustfmt) cargo fmt --version ;;
        cargo-deny) cargo deny --version ;;
        *) "$1" --version ;;
    esac
}

# Print the quoted string value of `key = "value"` from a tracked record.
record_string() {
    local record="$1" key="$2" line
    [ -r "${record}" ] || return 1
    while IFS= read -r line || [ -n "${line}" ]; do
        case "${line}" in
            "${key} = \""*\")
                line=${line#"${key} = \""}
                printf '%s\n' "${line%\"}"
                return 0
                ;;
        esac
    done <"${record}"
    return 1
}

# The pinned version of `tool`, read from the records under `root`.
pinned_tool_version() {
    local root="$1" tool="$2"
    case "${tool}" in
        rustc|cargo) record_string "${root}/${RUST_TOOLCHAIN_RECORD}" channel ;;
        *) record_string "${root}/${WORKFLOW_TOOL_RECORD}" "${tool}" ;;
    esac
}

# Print the pinned version of `tool` under `root`, or report that none exists.
require_pinned_version() {
    local root="$1" tool="$2"
    pinned_tool_version "${root}" "${tool}" || {
        printf 'ERROR: no pinned identity for workflow tool %s under %s\n' \
            "${tool}" "${root}" >&2
        return 1
    }
}

# Succeed when `executable` resolves on PATH; otherwise report `tool` missing.
require_present_executable() {
    local tool="$1" executable="$2"
    command -v "${executable}" >/dev/null 2>&1 || {
        printf 'INFRASTRUCTURE: required workflow tool is unavailable: %s\n' \
            "${tool}" >&2
        return 75
    }
}

require_workflow_tool() {
    local root="$1" tool="$2" executable pinned output reported status=0
    listed "${tool}" camber_workflow_tools || {
        printf 'ERROR: unknown workflow tool: %s\n' "${tool}" >&2
        return 64
    }
    executable=$(workflow_tool_executable "${tool}")
    require_present_executable "${tool}" "${executable}" || return $?
    listed "${tool}" camber_present_workflow_tools && return 0
    pinned=$(require_pinned_version "${root}" "${tool}") || return $?
    output=$(workflow_tool_version "${tool}" 2>&1) || status=$?
    case "${status}:${output}" in
        0:*) ;;
        *'is not installed'*)
            printf 'INFRASTRUCTURE: pinned workflow tool %s %s is not installed: %s\n' \
                "${tool}" "${pinned}" "${output}" >&2
            return 75
            ;;
        *)
            printf 'ERROR: workflow tool %s did not report its identity (exit %s): %s\n' \
                "${tool}" "${status}" "${output}" >&2
            return 1
            ;;
    esac
    reported=${output%%$'\n'*}
    case "${reported}" in
        "${tool} ${pinned}"|"${tool} ${pinned} "*) ;;
        *)
            printf 'ERROR: unapproved workflow tool identity: %s reported "%s"; pinned %s\n' \
                "${tool}" "${reported}" "${pinned}" >&2
            return 1
            ;;
    esac
    printf 'Workflow tool %s: %s [%s]\n' \
        "${tool}" "${reported}" "$(command -v "${executable}")"
}

# Admit each named tool, or every workflow tool when none is named, against
# the pinned identities recorded under `root`.
require_workflow_tools() {
    local root="$1" tool
    shift
    [ "$#" -gt 0 ] || while IFS= read -r tool; do
        set -- "$@" "${tool}"
    done < <(camber_workflow_tools)
    for tool in "$@"; do
        require_workflow_tool "${root}" "${tool}" || return $?
    done
}

# Install each named Cargo-built tool at the exact identity its record pins.
# Only CI calls this; reproduction admits installed tools and never installs.
install_workflow_tools() {
    local root="$1" tool pinned status
    shift
    [ "$#" -gt 0 ] || {
        printf 'ERROR: install-tools needs at least one tool\n' >&2
        return 64
    }
    require_present_executable cargo cargo || return $?
    for tool in "$@"; do
        listed "${tool}" camber_installed_workflow_tools || {
            printf 'ERROR: workflow tool is not installed through Cargo: %s\n' \
                "${tool}" >&2
            return 64
        }
        pinned=$(require_pinned_version "${root}" "${tool}") || return $?
        status=0
        cargo install --locked "${tool}" --version "${pinned}" || status=$?
        [ "${status}" -eq 0 ] || {
            printf 'ERROR: cargo install of workflow tool %s %s failed with status %s\n' \
                "${tool}" "${pinned}" "${status}" >&2
            return "${status}"
        }
    done
}

run_workflow_phase() {
    local phase="$1" checkout="$2"
    case "${phase}" in
        hook-contract) "${checkout}/.github/scripts/ci-selftest.sh" ;;
        fmt) cargo fmt --check ;;
        clippy)
            cargo clippy --workspace \
                --features "${CAMBER_WORKFLOW_FEATURES}" -- -D warnings
            ;;
        doc)
            cargo --config 'build.rustdocflags=["-D","warnings"]' doc --workspace --lib \
                --features "${CAMBER_WORKFLOW_FEATURES}" --no-deps
            ;;
        test)
            cargo test --workspace --exclude camber-bench \
                --features "${CAMBER_WORKFLOW_FEATURES}"
            ;;
        deny) cargo deny --workspace check ;;
        pedant-source) "${checkout}/.github/scripts/check-pedant.sh" source ;;
        pedant-tests) "${checkout}/.github/scripts/check-pedant.sh" tests ;;
        supply-chain) "${checkout}/.github/scripts/check-supply-chain.sh" ;;
        *)
            printf 'ERROR: unknown workflow phase: %s\n' "${phase}" >&2
            return 64
            ;;
    esac
}

# Run every phase in order from `checkout` and stop at the first failure with
# its status. Callers invoke this inside a conditional, where Bash suspends
# errexit, so each status is captured explicitly.
run_workflow_checks() {
    local checkout="$1" phase status
    cd "${checkout}" || return 75
    for phase in $(camber_workflow_checks); do
        printf '==> workflow phase %s\n' "${phase}"
        status=0
        run_workflow_phase "${phase}" "${checkout}" </dev/null || status=$?
        [ "${status}" -eq 0 ] || {
            printf 'ERROR: workflow phase %s failed with status %s\n' \
                "${phase}" "${status}" >&2
            return "${status}"
        }
    done
}

# Admit the checkout's pinned tools from inside the checkout, so rustup
# resolves the same toolchain the checks use, then run its checks under CI's
# compiler flags without moving the caller's working directory. Cargo prefers
# CARGO_ENCODED_RUSTFLAGS over RUSTFLAGS, so the ambient one is dropped.
run_workflow_checkout() {
    local checkout="$1"
    (
        cd "${checkout}" || exit 75
        unset CARGO_ENCODED_RUSTFLAGS
        export RUSTFLAGS="${CAMBER_WORKFLOW_RUSTFLAGS}"
        require_workflow_tools "${checkout}" || exit $?
        run_workflow_checks "${checkout}"
    )
}

remove_workflow_checkout() {
    local source_root="$1" temporary_root="$2" checkout="$3" status=0
    git -C "${source_root}" worktree remove --force "${checkout}" \
        >/dev/null || status=$?
    case "${temporary_root}" in
        "${TMPDIR:-/tmp}"/camber-workflow.*)
            rm -rf -- "${temporary_root}" || status=$?
            ;;
        *)
            printf 'ERROR: refusing to remove unexpected scratch root: %s\n' \
                "${temporary_root}" >&2
            return 1
            ;;
    esac
    git -C "${source_root}" worktree prune >/dev/null || status=$?
    return "${status}"
}

# The checks' status survives cleanup; a cleanup failure fails a clean run.
workflow_result() {
    local status="$1" cleanup_status="$2"
    [ "${cleanup_status}" -eq 0 ] || printf \
        'ERROR: workflow scratch checkout cleanup failed with status %s\n' \
        "${cleanup_status}" >&2
    case "${status}" in
        0) return "${cleanup_status}" ;;
        *) return "${status}" ;;
    esac
}

repository_root() {
    git rev-parse --show-toplevel 2>/dev/null || {
        printf 'INFRASTRUCTURE: cannot resolve the repository root\n' >&2
        return 75
    }
}

reproduce_ci_main() {
    local source_root porcelain temporary_root checkout status=0 cleanup_status=0
    source_root=$(repository_root) || return $?
    porcelain=$(git -C "${source_root}" status --porcelain=v1) || return 75
    [ -z "${porcelain}" ] || {
        printf 'ERROR: workflow reproduction requires a clean committed tree\n' >&2
        return 1
    }
    temporary_root=$(mktemp -d "${TMPDIR:-/tmp}/camber-workflow.XXXXXX") \
        || return 75
    checkout="${temporary_root}/checkout"
    git -C "${source_root}" worktree add --detach "${checkout}" HEAD >/dev/null \
        || { rm -rf -- "${temporary_root}"; return 75; }
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${source_root}/target/workflow-reproduction}"
    run_workflow_checkout "${checkout}" || status=$?
    remove_workflow_checkout \
        "${source_root}" "${temporary_root}" "${checkout}" || cleanup_status=$?
    workflow_result "${status}" "${cleanup_status}"
}

# CI jobs install their Cargo-built tools through `install-tools <tool>...`
# and admit every tool they use through `require-tools <tool>...`.
reproduce_ci_entry() {
    local root
    case "${1:-}" in
        '') reproduce_ci_main ;;
        install-tools)
            shift
            root=$(repository_root) || return $?
            install_workflow_tools "${root}" "$@"
            ;;
        require-tools)
            shift
            root=$(repository_root) || return $?
            require_workflow_tools "${root}" "$@"
            ;;
        *)
            printf 'Usage: %s [install-tools <tool>... | require-tools <tool>...]\n' \
                "$0" >&2
            return 64
            ;;
    esac
}

[ "${CAMBER_HOOK_LIBRARY_MODE:-0}" = 1 ] || reproduce_ci_entry "$@"
