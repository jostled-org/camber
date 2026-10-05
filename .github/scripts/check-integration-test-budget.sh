#!/usr/bin/env bash
#
# Measure the integration test budget: 100 exact repetitions of each
# deterministic integration case, bound to one committed tree and the pinned
# toolchain.
#
# The runner takes no arguments. It builds the three Cargo test roots once,
# lists each case exactly and requires one listed test, then runs each case
# 100 times as one exact libtest filter under its own root. Each sample runs
# with a fresh, runner-owned TMPDIR; anything left in it is resource residue.
# Cargo runs as PATH selects it, under the inherited CARGO_TARGET_DIR.
#
# The last line of stdout is one JSON object: tree_oid, rustc, host, profile,
# target_dir, executable_count, sample_count, per_case_duration_ms,
# target_bytes_before, target_bytes_after, owned_resource_residue, and
# result. sample_count counts the samples recorded across every case, so a
# refused or failed run reports only the samples that ran.
# executable_count counts all workspace integration test targets;
# a count above 30 fails before the runner builds its three sampled targets.
# An unreadable inventory reports zero and fails. Diagnostics go to
# stderr. Sample output goes only to bounded logs
# under CAMBER_BUDGET_LOG_DIR (default `<target dir>/integration-budget`).
#
# Exit 0 means every sample passed with no residue on a clean committed tree
# under the pinned toolchain. 75 means unavailable infrastructure, such as no
# Cargo. Any other status is a failed measurement.

set -euo pipefail

RUNNER_ROOT=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
readonly RUNNER_ROOT
# shellcheck source=.github/scripts/reproduce-ci.sh
CAMBER_HOOK_LIBRARY_MODE=1 source "${RUNNER_ROOT}/.github/scripts/reproduce-ci.sh"

readonly BUDGET_PACKAGE='camber'
readonly BUDGET_PROFILE='test'
readonly SAMPLE_COUNT=100
# The most one sample, the build, or a failure report keeps of its output.
readonly SAMPLE_LOG_BYTES=4096
readonly BUILD_LOG_BYTES=65536
readonly REPORT_TAIL_BYTES=4096
# The repeated cases: the Cargo test root, and the libtest filter inside it.
readonly CASE_ROOTS=(
    component_runtime_resources
    component_integrations
    acceptance_owned_lifecycle
)
readonly CASE_FILTERS=(
    integration_lifecycle::integration_admission_is_atomic_with_root_closure
    nats_operations::delivered_slow_consumer_event_closes_every_subscription
    dns_cleanup::dns_waiter_drop_keeps_provider_until_cleanup_is_named_or_deleted
)

# Measurement state the summary reports.
tree_oid=''
rustc_identity=''
host=''
target_dir="${CARGO_TARGET_DIR:-${RUNNER_ROOT}/target}"
target_bytes_before=0
residue=0
samples_recorded=0
executable_count=0
# Comma-separated sample durations, indexed like CASE_ROOTS.
case_durations=()
# The runner's own scratch root and log directory.
run_root=''
log_dir=''

report() {
    printf 'integration budget: %s\n' "$1" >&2
}

# Cargo test over the measured package, under the workflow feature set.
cargo_tests() {
    cargo test -p "${BUDGET_PACKAGE}" --features "${CAMBER_WORKFLOW_FEATURES}" "$@"
}

cargo_root() {
    local root="$1"
    shift
    cargo_tests --test "${root}" "$@"
}

# Print the last `bytes` of `file` to stderr, under a heading.
report_tail() {
    local heading="$1" file="$2" bytes="$3"
    printf -- '--- %s (last %s bytes) ---\n' "${heading}" "${bytes}" >&2
    tail -c "${bytes}" "${file}" >&2 || true
    printf '\n' >&2
}

# --- Summary -------------------------------------------------------------

# The size of the target directory in bytes.
target_bytes() {
    local kilobytes
    kilobytes=$(directory_kib "${target_dir}") || {
        report "cannot measure ${target_dir}"
        return 1
    }
    printf '%s\n' "$((kilobytes * 1024))"
}

per_case_json() {
    local index separator=''
    printf '{'
    for index in "${!CASE_ROOTS[@]}"; do
        printf '%s%s:[%s]' "${separator}" \
            "$(json_string "${CASE_ROOTS[index]}::${CASE_FILTERS[index]}")" \
            "${case_durations[index]-}"
        separator=','
    done
    printf '}'
}

write_summary() {
    local status="$1" bytes_after="$2"
    printf '{"tree_oid":%s,"rustc":%s,"host":%s,"profile":%s,"target_dir":%s,' \
        "$(json_string "${tree_oid}")" "$(json_string "${rustc_identity}")" \
        "$(json_string "${host}")" "$(json_string "${BUDGET_PROFILE}")" \
        "$(json_string "${target_dir}")"
    printf '"executable_count":%s,"sample_count":%s,"per_case_duration_ms":%s,' \
        "${executable_count}" "${samples_recorded}" "$(per_case_json)"
    printf '"target_bytes_before":%s,"target_bytes_after":%s,' \
        "${target_bytes_before}" "${bytes_after}"
    printf '"owned_resource_residue":%s,"result":%s}\n' \
        "${residue}" "$(json_string "$(evidence_status_name "${status}")")"
}

# --- Binding -------------------------------------------------------------

# Bind the measurement to the committed tree; uncommitted changes refuse it.
bind_tree() {
    local porcelain
    porcelain=$(git -C "${RUNNER_ROOT}" status --porcelain=v1) || {
        report 'cannot read the working tree state'
        return "${STATUS_INFRASTRUCTURE}"
    }
    [ -z "${porcelain}" ] || {
        report 'the working tree has uncommitted changes; a receipt names a committed tree'
        return 1
    }
    tree_oid=$(git -C "${RUNNER_ROOT}" rev-parse 'HEAD^{tree}') || {
        report 'cannot resolve the committed tree'
        return "${STATUS_INFRASTRUCTURE}"
    }
}

# Admit the pinned rustc and Cargo, then record the compiler and its host.
bind_toolchain() {
    local line
    require_workflow_tools "${RUNNER_ROOT}" cargo rustc >&2 || return $?
    rustc_identity=$(rustc --version) || {
        report 'rustc did not report its version'
        return "${STATUS_INFRASTRUCTURE}"
    }
    while IFS= read -r line; do
        case "${line}" in
            'host: '*) host=${line#host: } ;;
        esac
    done < <(rustc -vV)
    [ -n "${host}" ] || { report 'rustc reported no host'; return 1; }
}

# --- Owned scratch and logs ----------------------------------------------

open_run() {
    local scratch="${TMPDIR:-/tmp}" log_root
    mkdir -p "${scratch}" || return "${STATUS_INFRASTRUCTURE}"
    run_root=$(mktemp -d "${scratch%/}/camber-budget.XXXXXX") || {
        report "cannot create a scratch root under ${scratch}"
        return "${STATUS_INFRASTRUCTURE}"
    }
    log_root="${CAMBER_BUDGET_LOG_DIR:-${target_dir}/integration-budget}"
    mkdir -p "${log_root}" || return "${STATUS_INFRASTRUCTURE}"
    log_dir=$(mktemp -d "${log_root%/}/${tree_oid:-unbound}.XXXXXX") || {
        report "cannot create a log directory under ${log_root}"
        return "${STATUS_INFRASTRUCTURE}"
    }
    report "logs: ${log_dir}"
}

close_run() {
    [ -n "${run_root}" ] || return 0
    rm -rf -- "${run_root}"
    [ ! -e "${run_root}" ] || { report "${run_root} survived removal"; return 1; }
    run_root=''
}

# --- Build and selection -------------------------------------------------

# Count the workspace's integration targets, not just the repeated samples.
check_executable_budget() {
    command -v jq >/dev/null || { report 'jq is required to read Cargo metadata'; return "${STATUS_INFRASTRUCTURE}"; }
    cargo metadata --locked --no-deps --format-version=1 >"${run_root}/metadata.json" || {
        report 'Cargo could not enumerate workspace targets'
        return 1
    }
    executable_count=$(jq -er '
        .workspace_members as $members |
        [.packages[] | select(.id as $id | $members | index($id)) |
         .targets[] | select(.kind | index("test"))] | length | select(. > 0)
    ' "${run_root}/metadata.json") || {
        executable_count=0
        report 'Cargo returned no valid workspace test inventory'
        return 1
    }
    [ "${executable_count}" -le 30 ] || {
        report "workspace has ${executable_count} integration test executables; limit is 30"
        return 1
    }
}

build_roots() {
    local root arguments=() raw status=0
    for root in "${CASE_ROOTS[@]}"; do
        arguments+=(--test "${root}")
    done
    raw="${run_root}/build.out"
    cargo_tests "${arguments[@]}" --no-run >"${raw}" 2>&1 || status=$?
    tail -c "${BUILD_LOG_BYTES}" "${raw}" >"${log_dir}/build.log" || {
        report "cannot write ${log_dir}/build.log"
        return 1
    }
    [ "${status}" -eq 0 ] && return 0
    report "the build of ${CASE_ROOTS[*]} failed with status ${status}"
    report_tail 'build output' "${raw}" "${REPORT_TAIL_BYTES}"
    return 1
}

# Each case lists exactly one test, and that test is the case.
verify_selection() {
    local index root filter problem
    for index in "${!CASE_ROOTS[@]}"; do
        root=${CASE_ROOTS[index]} filter=${CASE_FILTERS[index]}
        problem=$(listing_problem "${root}::${filter}" "${filter}" \
            cargo_root "${root}" -- "${filter}" --exact --list \
            2>>"${log_dir}/list.log")
        [ -z "${problem}" ] || { report "${problem}"; return 1; }
    done
}

# --- Samples -------------------------------------------------------------

# Whole milliseconds from a `%3R` real-time report such as `1.234`.
milliseconds() {
    local reported seconds fraction
    reported=$(tr -d '[:space:]' <"$1")
    seconds=${reported%%.*} fraction=${reported#*.}
    printf '%s\n' "$((10#${seconds:-0} * 1000 + 10#${fraction:-0}))"
}

# Count and remove what a sample left in its own TMPDIR. A directory that
# cannot be walked counts as residue: its contents are unknown.
reclaim_sample_dir() {
    local directory="$1" leftover
    leftover=$(find "${directory}" -mindepth 1 | wc -l) || {
        report "cannot count the entries in ${directory}"
        leftover=$((leftover + 1))
    }
    leftover=$((leftover))
    rm -rf -- "${directory}"
    [ ! -e "${directory}" ] || {
        report "${directory} survived removal"
        leftover=$((leftover + 1))
    }
    printf '%s\n' "${leftover}"
}

# Run sample `sample` of case `index` once; record its duration and residue.
run_sample() {
    local index="$1" sample="$2" root filter directory raw timing status=0 elapsed leftover
    root=${CASE_ROOTS[index]} filter=${CASE_FILTERS[index]}
    directory=$(mktemp -d "${run_root}/sample.XXXXXX") || return "${STATUS_INFRASTRUCTURE}"
    raw="${run_root}/sample.out" timing="${run_root}/sample.time"
    {
        TIMEFORMAT=%3R
        time TMPDIR="${directory}" cargo_root "${root}" -- "${filter}" --exact >"${raw}" 2>&1
    } 2>"${timing}" || status=$?
    elapsed=$(milliseconds "${timing}")
    leftover=$(reclaim_sample_dir "${directory}")
    residue=$((residue + leftover))
    case_durations[index]="${case_durations[index]:+${case_durations[index]},}${elapsed}"
    samples_recorded=$((samples_recorded + 1))
    {
        printf '== sample %s: status %s, %s ms, residue %s\n' \
            "${sample}" "${status}" "${elapsed}" "${leftover}"
        tail -c "${SAMPLE_LOG_BYTES}" "${raw}"
        printf '\n'
    } >>"${log_dir}/${root}.log" || {
        report "cannot write ${log_dir}/${root}.log"
        return 1
    }
    sample_verdict "${root}::${filter}" "${sample}" "${status}" "${leftover}" "${raw}"
}

sample_verdict() {
    local selector="$1" sample="$2" status="$3" leftover="$4" raw="$5"
    case "${status}:${leftover}" in
        0:0) ;;
        0:*)
            report "${selector} sample ${sample} left ${leftover} entries in its TMPDIR"
            return 1
            ;;
        *)
            report "${selector} sample ${sample} failed with status ${status}"
            report_tail "${selector} sample ${sample}" "${raw}" "${REPORT_TAIL_BYTES}"
            return 1
            ;;
    esac
    ran_one_passing_test "$(<"${raw}")" || {
        report "${selector} sample ${sample} did not run exactly one passing test"
        report_tail "${selector} sample ${sample}" "${raw}" "${REPORT_TAIL_BYTES}"
        return 1
    }
}

run_samples() {
    local index sample
    for index in "${!CASE_ROOTS[@]}"; do
        for ((sample = 1; sample <= SAMPLE_COUNT; sample++)); do
            run_sample "${index}" "${sample}" || return $?
        done
        report "${CASE_ROOTS[index]}::${CASE_FILTERS[index]}: ${SAMPLE_COUNT} samples passed"
    done
}

# --- Measurement ---------------------------------------------------------

measure() {
    bind_tree || return $?
    bind_toolchain || return $?
    target_bytes_before=$(target_bytes) || return $?
    open_run || return $?
    check_executable_budget || return $?
    build_roots || return $?
    verify_selection || return $?
    run_samples
}

main() {
    local status=0 cleanup=0 bytes_after=0
    [ "$#" -eq 0 ] || { report 'takes no arguments'; return 64; }
    cd "${RUNNER_ROOT}"
    trap 'close_run || true' EXIT
    trap 'close_run || true; exit 130' INT TERM
    measure || status=$?
    close_run || cleanup=$?
    [ "${cleanup}" -eq 0 ] || [ "${status}" -ne 0 ] || status=1
    # An unmeasured target directory is missing data; it fails a clean run.
    bytes_after=$(target_bytes) || {
        bytes_after=0
        [ "${status}" -ne 0 ] || status=1
    }
    report "$(evidence_status_name "${status}")"
    write_summary "${status}" "${bytes_after}"
    return "${status}"
}

main "$@"
