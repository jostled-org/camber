#!/usr/bin/env bash

set -euo pipefail

camber_workflow_checks() {
    printf '%s\n' \
        hook-contract fmt clippy test deny pedant-source pedant-tests supply-chain
}

require_workflow_tools() {
    local tool
    for tool in git cargo cargo-deny pedant protoc; do
        command -v "${tool}" >/dev/null 2>&1 || {
            printf 'INFRASTRUCTURE: required workflow tool is unavailable: %s\n' \
                "${tool}" >&2
            return 75
        }
    done
}

run_workflow_checks() {
    local source_root="$1" checkout="$2"
    cd "${checkout}" || return 75
    "${source_root}/.github/scripts/plan-loop-hooks-selftest.sh"
    cargo fmt --check
    cargo clippy --workspace \
        --features 'profiling,ws,grpc,acme,dns01,nats,sqs,otel' -- -D warnings
    cargo test --workspace --exclude camber-bench \
        --features 'profiling,ws,grpc,acme,dns01,nats,sqs,otel'
    cargo deny --workspace check
    "${source_root}/.github/scripts/check-pedant.sh" source
    "${source_root}/.github/scripts/check-pedant.sh" tests
    "${source_root}/.github/scripts/check-supply-chain.sh"
}

remove_workflow_checkout() {
    local source_root="$1" temporary_root="$2" checkout="$3" status=0
    git -C "${source_root}" worktree remove --force "${checkout}" \
        >/dev/null 2>&1 || status=$?
    case "${temporary_root}" in
        "${TMPDIR:-/tmp}"/camber-workflow.*) rm -rf -- "${temporary_root}" ;;
        *) return 1 ;;
    esac
    return "${status}"
}

reproduce_ci_main() {
    local source_root temporary_root checkout status=0 cleanup_status=0
    source_root=$(git rev-parse --show-toplevel 2>/dev/null) || return 75
    require_workflow_tools || return $?
    [ -z "$(git -C "${source_root}" status --porcelain=v1)" ] || {
        printf 'ERROR: workflow reproduction requires a clean committed tree\n' >&2
        return 1
    }
    temporary_root=$(mktemp -d "${TMPDIR:-/tmp}/camber-workflow.XXXXXX") \
        || return 75
    checkout="${temporary_root}/checkout"
    git -C "${source_root}" worktree add --detach "${checkout}" HEAD >/dev/null \
        || { rmdir "${temporary_root}"; return 75; }
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${source_root}/target/workflow-reproduction}"
    run_workflow_checks "${source_root}" "${checkout}" || status=$?
    remove_workflow_checkout \
        "${source_root}" "${temporary_root}" "${checkout}" || cleanup_status=$?
    [ "${status}" -ne 0 ] && return "${status}"
    return "${cleanup_status}"
}

[ "${CAMBER_HOOK_LIBRARY_MODE:-0}" = 1 ] || reproduce_ci_main "$@"
