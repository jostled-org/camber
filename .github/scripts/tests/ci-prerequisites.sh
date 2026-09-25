#!/usr/bin/env bash

set -euo pipefail

ROOT=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
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
expect_message 'required workflow tool is unavailable: rg'
reject_message 'CI omits hook self-test'

expect_exit 75 'workflow reproduction without ripgrep' "${BASH_EXE}" -s -- \
    "${REPRODUCE}" "${FIXTURE}/bin" "${ROOT}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    git() { :; }
    PATH="$2"
    require_workflow_tools "$3"
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
            "--config build.rustdocflags=[\"-D\",\"warnings\"] doc --workspace --lib --features ${CAMBER_WORKFLOW_FEATURES} --no-deps")
                DOC_SEEN=1
                printf 'documentation command observed\n'
                return "${DOC_STATUS}"
                ;;
            'deny --workspace check') printf 'post-documentation phase observed\n' ;;
        esac
        return 0
    }
    status=0
    run_workflow_checks "$2" || status=$?
    [ "${DOC_SEEN}" = 1 ] || exit 98
    exit "${status}"
BASH
    expect_message 'documentation command observed'
    case "${doc_status}" in
        0) expect_message 'post-documentation phase observed' ;;
        *) reject_message 'post-documentation phase observed' ;;
    esac
done

# The versions the tracked records pin; an absent pin fails the run.
CAMBER_HOOK_LIBRARY_MODE=1 source "${ROOT}/.github/scripts/reproduce-ci.sh"
CARGO_DENY_PIN=$(require_pinned_version "${ROOT}" cargo-deny)
PEDANT_PIN=$(require_pinned_version "${ROOT}" pedant)

expect_exit 0 'pinned tool install' \
    "${BASH_EXE}" -s -- "${REPRODUCE}" "${ROOT}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    cargo() { printf 'cargo %s\n' "$*"; }
    install_workflow_tools "$2" cargo-deny pedant
BASH
expect_message "cargo install --locked cargo-deny --version ${CARGO_DENY_PIN}"
expect_message "cargo install --locked pedant --version ${PEDANT_PIN}"

expect_exit 64 'install of a rustup-managed tool' \
    "${BASH_EXE}" -s -- "${REPRODUCE}" "${ROOT}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    cargo() { printf 'cargo %s\n' "$*"; }
    install_workflow_tools "$2" clippy
BASH
expect_message 'workflow tool is not installed through Cargo: clippy'
reject_message 'cargo install'

expect_exit 64 'install without a tool' \
    "${BASH_EXE}" -s -- "${REPRODUCE}" "${ROOT}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    install_workflow_tools "$2"
BASH
expect_message 'install-tools needs at least one tool'

expect_exit 101 'failed tool install' \
    "${BASH_EXE}" -s -- "${REPRODUCE}" "${ROOT}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    cargo() { printf 'cargo %s\n' "$*"; return 101; }
    install_workflow_tools "$2" cargo-deny pedant
BASH
expect_message 'cargo install of workflow tool cargo-deny'
reject_message 'install --locked pedant'

expect_exit 75 'tool install without cargo' "${BASH_EXE}" -s -- \
    "${REPRODUCE}" "${FIXTURE}/bin" "${ROOT}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    PATH="$2"
    install_workflow_tools "$3" pedant
BASH
expect_message 'required workflow tool is unavailable: cargo'

expect_exit 1 'reviewed dependency base is not a commit' \
    "${BASH_EXE}" -s -- "${ROOT}/.github/scripts/check-supply-chain.sh" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    CAMBER_DEPENDENCY_BASE_SHA=reviewed-base
    git() { return 1; }
    report_reviewed_input_changes
BASH
expect_message 'reviewed dependency base is not a commit: reviewed-base'

expect_exit 1 'missing dependency input record' \
    "${BASH_EXE}" -s -- "${ROOT}/.github/scripts/check-supply-chain.sh" "${FIXTURE}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    cd "$2"
    validate_dependency_inputs
BASH
expect_message 'reviewed dependency input record is missing'

expect_exit 0 'consumed tool record' "${BASH_EXE}" -s -- \
    "${SELFTEST}" "${ROOT}/${WORKFLOW_TOOL_RECORD}" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    [ -z "$(unconsumed_tool_records "$2")" ]
BASH

cat "${ROOT}/${WORKFLOW_TOOL_RECORD}" - >"${FIXTURE}/workflow-tools.toml" <<'TOML'
cargo-nextest = "0.9.0"
pedant 0.30.1
TOML
expect_exit 0 'unconsumed tool record' "${BASH_EXE}" -s -- \
    "${SELFTEST}" "${FIXTURE}/workflow-tools.toml" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    unconsumed_tool_records "$2"
BASH
expect_message 'no pinned workflow tool consumes record entry: cargo-nextest'
expect_message 'malformed workflow tool record line: pedant 0.30.1'

expect_exit 0 'pinned workflow contract' "${BASH_EXE}" -s -- \
    "${SELFTEST}" "${ROOT}/.github/workflows/ci.yml" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    check_workflow_contract "$2"
BASH

# Each mutation breaks one clause of the workflow tool contract.
expect_workflow_violation() {
    local label="$1" expression="$2" diagnostic="$3"
    sed -e "${expression}" "${ROOT}/.github/workflows/ci.yml" >"${FIXTURE}/ci.yml"
    cmp -s "${ROOT}/.github/workflows/ci.yml" "${FIXTURE}/ci.yml" \
        && { printf '%s: mutation changed nothing\n' "${label}" >&2; exit 1; }
    expect_exit 1 "${label}" "${BASH_EXE}" -s -- "${SELFTEST}" "${FIXTURE}/ci.yml" <<'BASH'
    CAMBER_HOOK_LIBRARY_MODE=1 source "$1"
    check_workflow_contract "$2"
BASH
    expect_message "${diagnostic}"
}

expect_workflow_violation 'omitted identity guard' \
    '/require-tools rustc cargo pedant/d' \
    'job supply-chain does not admit pedant through its identity guard'
expect_workflow_violation 'guard omits a used tool' \
    's/require-tools git rg rustc cargo clippy rustfmt/require-tools git rg rustc cargo clippy/' \
    'job check does not admit rustfmt through its identity guard'
expect_workflow_violation 'install of an unpinned tool' \
    's/install-tools pedant$/install-tools pedant cargo-nextest/' \
    'job supply-chain installs cargo-nextest, which is not a pinned Cargo-installed tool'
expect_workflow_violation 'direct cargo install' \
    's/\.github\/scripts\/reproduce-ci\.sh install-tools cargo-deny/cargo install --locked cargo-deny --version 0.19.0/' \
    'installs a tool outside reproduce-ci.sh install-tools: - run: cargo install --locked cargo-deny'
expect_workflow_violation 'install after the identity guard' \
    '/install-tools cargo-deny/{h;d;};/require-tools rustc cargo cargo-deny/G' \
    'job dependency-policy installs a tool after its identity guard'
expect_workflow_violation 'drifted Pedant matrix' \
    's/package: \[camber, camber-bench,/package: [camber,/' \
    'Pedant matrix drifted from the hook inventory'
expect_workflow_violation 'floating toolchain' \
    's/rustup toolchain install$/rustup toolchain install stable/' \
    'installs a toolchain rust-toolchain.toml does not pin'
expect_workflow_violation 'floating action tag' \
    's/checkout@[0-9a-f]* # v4/checkout@v4/' \
    'action is not pinned to a commit: - uses: actions/checkout@v4'

printf 'CI prerequisite regression tests: PASS\n'
