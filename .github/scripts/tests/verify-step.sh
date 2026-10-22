#!/usr/bin/env bash
set -euo pipefail
# The caller's routing inputs must not reach these rows.
unset CAMBER_VERIFY_STEP CAMBER_VERIFY_CHANGED_FILES
root=$(cd "$(dirname "$0")/../../.." && pwd -P)
source_script=${1:-$root/.github/scripts/verify-step.sh}
scratch=$(mktemp -d "${TMPDIR:-/tmp}/camber-verify-step.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/repo/.github/scripts" "$scratch/repo/docs/scripts" "$scratch/bin"
cp "$source_script" "$scratch/repo/.github/scripts/verify-step.sh"
cat > "$scratch/bin/cargo" <<'SH'
#!/usr/bin/env bash
printf 'cargo %s\n' "$*" >> "$CALLS"
if [ "cargo $1" = "$FAIL_COMMAND" ]; then printf '%s' "${FAIL_STDERR:-}" >&2; exit "$FAIL_STATUS"; fi
exit 0
SH
cat > "$scratch/repo/.github/scripts/check-pedant.sh" <<'SH'
#!/usr/bin/env bash
printf 'pedant %s\n' "$*" >> "$CALLS"
if [ "pedant $1" = "$FAIL_COMMAND" ]; then exit "$FAIL_STATUS"; fi
exit 0
SH
cat > "$scratch/repo/docs/scripts/with_build_lease.sh" <<'SH'
#!/usr/bin/env bash
printf 'lease %s\n' "$*" >> "$CALLS"
exit 73
SH
chmod +x "$scratch/bin/cargo" "$scratch/repo/.github/scripts/check-pedant.sh" "$scratch/repo/docs/scripts/with_build_lease.sh"
export PATH="$scratch/bin:$PATH" CALLS="$scratch/calls" FAIL_STATUS=75
assert_compile_before_test() {
    local selection=$1 features=$2 check_line test_line
    check_line=$(rg -nFx "cargo check $selection --tests --keep-going$features" "$CALLS" | cut -d: -f1) || {
        cat "$CALLS"
        printf 'FAIL: missing complete test-compilation check for %s\n' "$selection"
        exit 1
    }
    test_line=$(rg -nFx "cargo test $selection$features" "$CALLS" | cut -d: -f1)
    [ "$check_line" -lt "$test_line" ] || {
        printf 'FAIL: compilation check must precede test execution for %s\n' "$selection"
        exit 1
    }
}
for scope in '' 'camber' 'camber-build camber'; do
    : > "$CALLS"
    status=0
    CAMBER_VERIFY_SCOPE="$scope" FAIL_COMMAND=none bash "$scratch/repo/.github/scripts/verify-step.sh" > "$scratch/output" 2>&1 || status=$?
    [ "$status" -eq 0 ] || { cat "$scratch/output"; cat "$CALLS"; exit 1; }
    if rg -q '^lease ' "$CALLS"; then
        printf 'FAIL: verification called a build lease\n'
        exit 1
    fi
    rg -q '^cargo clippy ' "$CALLS"
    # Clippy lints the tests and benches too, not only the library source.
    if rg '^cargo clippy ' "$CALLS" | rg -qv -- ' --all-targets '; then
        cat "$CALLS"
        printf 'FAIL: a clippy run skipped the test targets\n'
        exit 1
    fi
    rg -q '^cargo test ' "$CALLS"
    case "$scope" in
        '') assert_compile_before_test '--workspace' ' --features profiling,ws,grpc,acme,dns01,nats,sqs,otel' ;;
        *)
            for pkg in $scope; do
                case "$pkg" in
                    camber) assert_compile_before_test "-p $pkg" ' --features profiling,ws,grpc,acme,dns01,nats,sqs,otel' ;;
                    *) assert_compile_before_test "-p $pkg" '' ;;
                esac
            done
            ;;
    esac
    : > "$CALLS"
    status=0
    CAMBER_VERIFY_SCOPE="$scope" FAIL_COMMAND='cargo check' FAIL_STATUS=101 \
        bash "$scratch/repo/.github/scripts/verify-step.sh" > "$scratch/output" 2>&1 || status=$?
    [ "$status" -eq 101 ] || { cat "$scratch/output"; printf 'FAIL: compile error exited %s\n' "$status"; exit 1; }
    rg -q '^cargo test ' "$CALLS" || { cat "$CALLS"; printf 'FAIL: compile error skipped test execution\n'; exit 1; }
    for failure in 'cargo fmt' 'pedant source' 'pedant tests' 'cargo clippy' 'cargo check' 'cargo test'; do
        : > "$CALLS"
        status=0
        CAMBER_VERIFY_SCOPE="$scope" FAIL_COMMAND="$failure" bash "$scratch/repo/.github/scripts/verify-step.sh" > "$scratch/output" 2>&1 || status=$?
        [ "$status" -eq 75 ] || { cat "$scratch/output"; exit 1; }
        last=$(tail -1 "$CALLS")
        case "$last" in "$failure"*) ;; *) printf 'FAIL: %s continued after %s: %s\n' "$scope" "$failure" "$last"; exit 1 ;; esac
        rg -q 'INFRASTRUCTURE' "$scratch/output" || { printf 'FAIL: missing infrastructure diagnostic\n'; exit 1; }
        case "$failure" in
            pedant*) named="check-pedant.sh ${failure#pedant }" ;;
            *) named=$failure ;;
        esac
        rg -qF "$named" "$scratch/output" || { printf 'FAIL: missing command identity: %s\n' "$named"; exit 1; }
    done
done
# Ordinary findings still aggregate; later checks must run without replacing the first failure.
status=0
: > "$CALLS"
CAMBER_VERIFY_SCOPE=camber FAIL_COMMAND='cargo fmt' FAIL_STATUS=23 bash "$scratch/repo/.github/scripts/verify-step.sh" > "$scratch/output" 2>&1 || status=$?
[ "$status" -eq 23 ]
rg -q '^cargo test ' "$CALLS"
CAMBER_VERIFY_SCOPE=camber FAIL_COMMAND=none bash "$scratch/repo/.github/scripts/verify-step.sh" > "$scratch/output" 2>&1
# Missing Cargo is unavailable infrastructure, not an unknown crate or a finding.
# Some distributions put a rustup proxy at /usr/bin/cargo, so the row mirrors
# the system tools without Cargo instead of trusting /usr/bin to lack it.
mkdir "$scratch/no-cargo"
find /usr/bin /bin -maxdepth 1 ! -name cargo ! -type d \
    -exec sh -c 'ln -sf "$@" "$0"' "$scratch/no-cargo" {} +
for scope in '' 'camber'; do
    status=0
    PATH="$scratch/no-cargo" CAMBER_VERIFY_SCOPE="$scope" FAIL_COMMAND=none \
        "$BASH" "$scratch/repo/.github/scripts/verify-step.sh" > "$scratch/output" 2>&1 || status=$?
    [ "$status" -eq 75 ] || { cat "$scratch/output"; printf 'FAIL: [%s] without cargo exited %s\n' "$scope" "$status"; exit 1; }
    rg -q 'INFRASTRUCTURE: cargo is not on PATH' "$scratch/output" || { printf 'FAIL: missing cargo diagnostic\n'; exit 1; }
done
# Scope names split on whitespace and never expand as globs.
: > "$CALLS"
touch "$scratch/repo/camber-glob"
CAMBER_VERIFY_SCOPE='camber-*' FAIL_COMMAND=none bash "$scratch/repo/.github/scripts/verify-step.sh" > "$scratch/output" 2>&1
rg -qF 'cargo pkgid -p camber-*' "$CALLS" || { cat "$CALLS"; printf 'FAIL: scope expanded as a glob\n'; exit 1; }
# Only Cargo's no-match answer is an unknown crate. Any other scope-lookup
# failure keeps its status and its diagnostic.
status=0
CAMBER_VERIFY_SCOPE=nosuch FAIL_COMMAND='cargo pkgid' FAIL_STATUS=101 \
    FAIL_STDERR='error: package ID specification `nosuch` did not match any packages' \
    bash "$scratch/repo/.github/scripts/verify-step.sh" > "$scratch/output" 2>&1 || status=$?
[ "$status" -eq 64 ] || { cat "$scratch/output"; printf 'FAIL: unknown crate exited %s\n' "$status"; exit 1; }
rg -qF 'unknown Rust crate(s): nosuch' "$scratch/output" || { cat "$scratch/output"; printf 'FAIL: missing unknown-crate diagnostic\n'; exit 1; }
status=0
CAMBER_VERIFY_SCOPE=camber FAIL_COMMAND='cargo pkgid' FAIL_STATUS=101 \
    FAIL_STDERR='error: failed to parse manifest' \
    bash "$scratch/repo/.github/scripts/verify-step.sh" > "$scratch/output" 2>&1 || status=$?
[ "$status" -eq 101 ] || { cat "$scratch/output"; printf 'FAIL: scope lookup failure exited %s\n' "$status"; exit 1; }
rg -qF 'failed to parse manifest' "$scratch/output" || { cat "$scratch/output"; printf 'FAIL: scope lookup diagnostic was discarded\n'; exit 1; }
# With no scope, the step number and the changed-files list select the run.
workspace_test='cargo test --workspace --features profiling,ws,grpc,acme,dns01,nats,sqs,otel'
route() {
    : > "$CALLS"
    status=0
    CAMBER_VERIFY_SCOPE='' CAMBER_VERIFY_STEP="$1" CAMBER_VERIFY_CHANGED_FILES="$2" FAIL_COMMAND=none \
        bash "$scratch/repo/.github/scripts/verify-step.sh" > "$scratch/output" 2>&1 || status=$?
    [ "$status" -eq 0 ] || { cat "$scratch/output"; printf 'FAIL: routing row exited %s\n' "$status"; exit 1; }
}
# A change confined to documents has nothing to verify.
printf 'docs/reference/client.md\n' > "$scratch/changed"
route 2 "$scratch/changed"
[ ! -s "$CALLS" ] || { cat "$CALLS"; printf 'FAIL: a docs-only change ran a check\n'; exit 1; }
rg -qF 'nothing to verify' "$scratch/output" || { cat "$scratch/output"; printf 'FAIL: missing skip notice\n'; exit 1; }
# The first step runs the workspace whatever the list says.
route 1 "$scratch/changed"
rg -qFx "$workspace_test" "$CALLS" || { cat "$CALLS"; printf 'FAIL: the first step skipped the workspace run\n'; exit 1; }
# Every input to a check selects the workspace run, beside any other change.
for changed in crates/camber/src/lib.rs Cargo.toml Cargo.lock rust-toolchain.toml .pedant.toml \
    .pedant/streaming-multipart.toml .github/scripts/check-pedant.sh; do
    printf 'docs/reference/client.md\n%s\n' "$changed" > "$scratch/changed"
    route 2 "$scratch/changed"
    rg -qFx "$workspace_test" "$CALLS" || { cat "$CALLS"; printf 'FAIL: a change to %s skipped the workspace run\n' "$changed"; exit 1; }
done
# A list that does not exist is an unknown diff.
route 2 "$scratch/absent"
rg -qFx "$workspace_test" "$CALLS" || { cat "$CALLS"; printf 'FAIL: an unknown diff skipped the workspace run\n'; exit 1; }
printf 'verify-step self-test: PASS\n'
