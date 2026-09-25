#!/usr/bin/env bash

set -euo pipefail

ROOT=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CAMBER_HOOK_LIBRARY_MODE=1 source "${ROOT}/.github/scripts/reproduce-ci.sh"
CAMBER_HOOK_LIBRARY_MODE=1 source "${ROOT}/.github/scripts/check-pedant.sh"

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

    assert_output \
        $'camber\ncamber-bench\ncamber-build\ncamber-cli\ncamber-macros' \
        "$(camber_pedant_packages)" \
        'Pedant package inventory drifted'

    CAMBER_HOOK_LIBRARY_MODE=1 source "${ROOT}/.github/scripts/verify-packages.sh"
    assert_output \
        $'camber-build\ncamber-macros\ncamber\ncamber-cli' \
        "$(camber_publishable_packages)" \
        'publishable package inventory drifted'

    assert_output \
        $'hook-contract\nfmt\nclippy\ndoc\ntest\ndeny\npedant-source\npedant-tests\nsupply-chain' \
        "$(camber_workflow_checks)" \
        'workflow check inventory drifted'
}

check_workflow_entries() {
    # Each phase command reproduce-ci.sh runs, as CI states it.
    assert_workflow_entry 'run: cargo fmt --check' 'format check' || return $?
    assert_workflow_entry \
        "run: cargo clippy --workspace --features \"\${CAMBER_CI_FEATURES}\" -- -D warnings" \
        'workspace lint check' || return $?
    assert_workflow_entry 'cargo --config '\''build.rustdocflags=["-D","warnings"]'\'' doc' \
        'warning-denying API documentation check' || return $?
    assert_workflow_entry 'cargo test --workspace --exclude camber-bench' \
        'workspace test run' || return $?
    assert_workflow_entry 'run: cargo deny --workspace check' \
        'dependency policy check' || return $?
    assert_workflow_entry "CAMBER_CI_FEATURES: ${CAMBER_WORKFLOW_FEATURES}" \
        'the feature set reproduce-ci.sh builds' || return $?
    assert_workflow_entry "RUSTFLAGS: ${CAMBER_WORKFLOW_RUSTFLAGS}" \
        'the compiler flags reproduce-ci.sh builds with' || return $?
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

# Strip leading whitespace from the caller's `line` without a subshell.
strip_indent() {
    line=${line#"${line%%[![:space:]]*}"}
}

# Print every Pedant matrix in `workflow` that does not list the Pedant hook's
# inventory, or a notice when the workflow has no such matrix.
pedant_matrix_violations() {
    local workflow="$1" line package expected='' matrices=0
    while IFS= read -r package; do
        expected="${expected:+${expected}, }${package}"
    done < <(camber_pedant_packages)
    expected="package: [${expected}]"
    while IFS= read -r line || [ -n "${line}" ]; do
        strip_indent
        case "${line}" in
            'package: ['*)
                matrices=$((matrices + 1))
                [ "${line}" = "${expected}" ] \
                    || printf 'Pedant matrix drifted from the hook inventory: %s\n' "${line}"
                ;;
        esac
    done <"${workflow}"
    [ "${matrices}" -gt 0 ] || printf 'CI omits the Pedant package matrix\n'
}

# The tracked dependency record must list exactly the hook's input inventory.
# The supply-chain hook owns the hash comparison.
check_dependency_input_inventory() {
    local record line recorded expected
    CAMBER_HOOK_LIBRARY_MODE=1 source "${ROOT}/.github/scripts/check-supply-chain.sh"
    record="${ROOT}/${DEPENDENCY_INPUT_RECORD}"
    [ -f "${record}" ] || fail 'dependency input record is missing'
    recorded=$(
        while IFS= read -r line || [ -n "${line}" ]; do
            printf '%s\n' "${line:66}"
        done <"${record}"
    )
    expected=$(cd "${ROOT}" && dependency_input_paths) || return 75
    assert_output "${expected}" "${recorded}" 'dependency input record inventory drifted'
}

# Print every entry in the tool `record` that no pinned workflow tool reads.
unconsumed_tool_records() {
    local record="$1" line
    while IFS= read -r line || [ -n "${line}" ]; do
        case "${line}" in
            ''|'#'*) ;;
            *' = "'*\")
                listed "${line%% = *}" camber_pinned_workflow_tools \
                    || printf 'no pinned workflow tool consumes record entry: %s\n' \
                        "${line%% = *}"
                ;;
            *) printf 'malformed workflow tool record line: %s\n' "${line}" ;;
        esac
    done <"${record}"
}

check_tool_records() {
    local tool violations
    violations=$(unconsumed_tool_records "${ROOT}/${WORKFLOW_TOOL_RECORD}") \
        || return $?
    [ -z "${violations}" ] || fail "workflow tool record drifted:"$'\n'"${violations}"
    for tool in $(camber_pinned_workflow_tools); do
        pinned_tool_version "${ROOT}" "${tool}" >/dev/null \
            || fail "no pinned identity for workflow tool ${tool}"
    done
    for tool in $(camber_installed_workflow_tools); do
        listed "${tool}" camber_pinned_workflow_tools \
            || fail "Cargo-installed workflow tool ${tool} is not a pinned tool"
    done
}

# Print every action reference not pinned to a full commit.
unpinned_actions() {
    local workflow="$1" line pinned='^(- )?uses: [^@ ]+@[0-9a-f]{40} # [^ ]+$'
    while IFS= read -r line || [ -n "${line}" ]; do
        strip_indent
        case "${line}" in
            '#'*) ;;
            *uses:*)
                [[ ${line} =~ ${pinned} ]] \
                    || printf 'action is not pinned to a commit: %s\n' "${line}"
                ;;
        esac
    done <"${workflow}"
}

# Append `tool` to the caller's `needs` unless already present, without a
# subshell.
add_need() {
    case " ${needs} " in
        *" $1 "*) ;;
        *) needs="${needs:+${needs} }$1" ;;
    esac
}

report_unguarded_tools() {
    local job="$1" needs="$2" guarded="$3" tool
    for tool in ${needs}; do
        case " ${guarded} " in
            *" ${tool} "*) ;;
            *) printf 'job %s does not admit %s through its identity guard\n' "${job}" "${tool}" ;;
        esac
    done
}

# Set the caller's `used` to the tools the workflow command in `line` runs,
# without a subshell.
command_tools() {
    case "${line}" in
        *'cargo fmt'*) used='rustc cargo rustfmt' ;;
        *'cargo clippy'*) used='rustc cargo clippy' ;;
        *'cargo deny'*) used='rustc cargo cargo-deny' ;;
        *'cargo '*) used='rustc cargo' ;;
        *check-pedant.sh*|*check-supply-chain.sh*) used='pedant' ;;
        *ci-selftest.sh*) used='git rg' ;;
        *) used='' ;;
    esac
}

# Print every job that installs or runs a tool its identity guard does not
# admit, installs after the guard, or installs anything but a pinned identity.
workflow_tool_violations() {
    local workflow="$1" line job='' needs='' guarded='' guard_seen=0 in_jobs=0
    local tool used
    local job_header='^  ([A-Za-z0-9_-]+):$'
    local install='^- run: \.github/scripts/reproduce-ci\.sh install-tools (.+)$'
    local guard='^- run: \.github/scripts/reproduce-ci\.sh require-tools (.+)$'
    while IFS= read -r line || [ -n "${line}" ]; do
        [ "${line}" != 'jobs:' ] || { in_jobs=1; continue; }
        [ "${in_jobs}" = 1 ] || continue
        if [[ ${line} =~ ${job_header} ]]; then
            report_unguarded_tools "${job}" "${needs}" "${guarded}"
            job=${BASH_REMATCH[1]} needs='' guarded='' guard_seen=0
            continue
        fi
        strip_indent
        case "${line}" in
            '#'*) continue ;;
            *'reproduce-ci.sh require-tools'*)
                guard_seen=1 guarded=''
                [[ ${line} =~ ${guard} ]] && guarded=${BASH_REMATCH[1]} \
                    || printf 'job %s has a malformed identity guard: %s\n' "${job}" "${line}"
                continue
                ;;
            *rustup*)
                [ "${line}" = '- run: rustup toolchain install' ] \
                    || printf 'job %s installs a toolchain rust-toolchain.toml does not pin: %s\n' \
                        "${job}" "${line}"
                used='rustc cargo'
                ;;
            *'reproduce-ci.sh install-tools'*)
                [[ ${line} =~ ${install} ]] || {
                    printf 'job %s has a malformed install entry: %s\n' "${job}" "${line}"
                    continue
                }
                used='rustc cargo'
                for tool in ${BASH_REMATCH[1]}; do
                    listed "${tool}" camber_installed_workflow_tools \
                        || printf 'job %s installs %s, which is not a pinned Cargo-installed tool\n' \
                            "${job}" "${tool}"
                    used="${used} ${tool}"
                done
                ;;
            *'cargo install'*)
                printf 'job %s installs a tool outside reproduce-ci.sh install-tools: %s\n' \
                    "${job}" "${line}"
                continue
                ;;
            *)
                command_tools
                [ -n "${used}" ] || continue
                [ "${guard_seen}" = 1 ] \
                    || printf 'job %s runs %s before its identity guard\n' "${job}" "${used}"
                ;;
        esac
        case "${line}:${guard_seen}" in
            *rustup*:1|*'reproduce-ci.sh install-tools'*:1)
                printf 'job %s installs a tool after its identity guard\n' "${job}"
                ;;
        esac
        for tool in ${used}; do
            add_need "${tool}"
        done
    done <"${workflow}"
    report_unguarded_tools "${job}" "${needs}" "${guarded}"
}

# Fail when `workflow` breaks the tool-identity contract.
check_workflow_contract() {
    local workflow="$1" violations
    violations=$(
        unpinned_actions "${workflow}" \
            && workflow_tool_violations "${workflow}" \
            && pedant_matrix_violations "${workflow}"
    ) || return $?
    [ -z "${violations}" ] || fail "workflow tool contract violated:"$'\n'"${violations}"
}

ci_selftest_main() {
    require_present_executable rg rg || return $?
    check_hook_inventories || return $?
    check_workflow_entries || return $?
    check_tool_records || return $?
    check_dependency_input_inventory || return $?
    check_workflow_contract "${ROOT}/.github/workflows/ci.yml" || return $?
    bash "${ROOT}/.github/scripts/tests/ci-prerequisites.sh" || return $?
    printf 'CI self-test: PASS\n'
}

[ "${CAMBER_HOOK_LIBRARY_MODE:-0}" = 1 ] || ci_selftest_main "$@"
