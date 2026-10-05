#!/usr/bin/env bash
set -euo pipefail
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
    rg -q '^cargo test ' "$CALLS"
    for failure in 'cargo fmt' 'pedant source' 'pedant tests' 'cargo clippy' 'cargo test'; do
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
for scope in '' 'camber'; do
    status=0
    PATH=/usr/bin:/bin CAMBER_VERIFY_SCOPE="$scope" FAIL_COMMAND=none \
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
printf 'verify-step self-test: PASS\n'
