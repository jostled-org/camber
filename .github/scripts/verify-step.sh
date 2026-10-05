#!/bin/bash
# Scoped verification for the Camber Rust workspace.
#
# Inputs:
#   CAMBER_VERIFY_SCOPE         — space-separated workspace crate names
#   CAMBER_VERIFY_STEP          — invocation number; 1 forces workspace fallback
#   CAMBER_VERIFY_CHANGED_FILES — file containing changed paths, one per line
#
# Routing precedence:
#   1. `CAMBER_VERIFY_SCOPE` non-empty → run fmt + pedant + clippy + test scoped to the
#      named crates only.
#   2. Scope empty but the changed-files list shows Rust-relevant
#      changes → full workspace run.
#   3. Both empty (first step, or no diff yet) → full workspace run, so the
#      first invocation cannot skip checks because its changes are unknown.
#   4. Scope empty and only non-Rust files changed (docs, plans) → nothing to
#      verify; exit 0.
#
# Feature flags: CI pins the camber crate's feature set
# (profiling,ws,grpc,acme,dns01,nats,sqs,otel). Only the `camber` crate
# declares those features, so scoped runs apply the list to `camber` and use
# default features for every other crate; workspace runs reuse the exact [ci]
# invocation. Scoped verification exercises the same features as CI.
#
# Infrastructure status 75 stops this invocation immediately and names the command.
# The caller decides recovery; this script promises no automatic retry.
# Ordinary failures retain the first nonzero status while remaining checks run.
#
# Exit code: 0 = pass, 64 = configuration error, 75 = infrastructure unavailable,
# other non-zero = verification failure.

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd) || exit 75
cd "$ROOT" || exit 75

# Mirror of the CI feature list; ci-selftest.sh pins it. Applied only to the
# camber crate — the other workspace members declare none of these features
# and cargo rejects unknown feature names on scoped invocations.
CAMBER_FEATURES="profiling,ws,grpc,acme,dns01,nats,sqs,otel"
FEATURED_CRATE="camber"

# ---------- aggregate exit ----------
worst=0
record_exit() {
    local code=$1
    if [ "$code" -eq 0 ]; then
        return 0
    fi
    if [ "$worst" -eq 0 ]; then
        worst=$code
    fi
    return 0
}
run() {
    "$@"
    local code=$?
    if [ "$code" -eq 75 ]; then
        printf '[verify_step] INFRASTRUCTURE: command unavailable or incomplete (status 75):' >&2
        printf ' %q' "$@" >&2
        printf '\n[verify_step] Stopped; later checks did not run. Resolve the prerequisite before retrying.\n' >&2
        exit 75
    fi
    record_exit "$code"
}
# Every check runs through Cargo; without it no verdict is possible.
require_cargo() {
    command -v cargo >/dev/null 2>&1 && return 0
    printf '[verify_step] INFRASTRUCTURE: cargo is not on PATH (status 75)\n' >&2
    exit 75
}

# ---------- parse and validate crate scope ----------
# Split crate names on whitespace, without glob expansion.
read -r -a scope_rust <<< "${CAMBER_VERIFY_SCOPE:-}"
if [ ${#scope_rust[@]} -gt 0 ]; then
    require_cargo
    missing=()
    for pkg in "${scope_rust[@]}"; do
        # Only Cargo's no-match answer names an unknown crate. Any other
        # failure keeps its status and its diagnostic.
        lookup_status=0
        lookup_error=$(cargo pkgid -p "$pkg" 2>&1 >/dev/null) || lookup_status=$?
        case "$lookup_status:$lookup_error" in
            0:*) ;;
            *'did not match any packages'*) missing+=("$pkg") ;;
            *)
                printf '[verify_step] scope lookup for %s failed (status %s):\n%s\n' \
                    "$pkg" "$lookup_status" "$lookup_error" >&2
                exit "$lookup_status"
                ;;
        esac
    done
    if [ ${#missing[@]} -gt 0 ]; then
        echo "ERROR: CAMBER_VERIFY_SCOPE names unknown Rust crate(s): ${missing[*]}" >&2
        exit 64
    fi
fi

# ---------- changed-files signal (scope-empty fallback) ----------
changed_rust=false
if [ -n "${CAMBER_VERIFY_CHANGED_FILES:-}" ] && [ -f "${CAMBER_VERIFY_CHANGED_FILES}" ]; then
    while IFS= read -r path; do
        case "$path" in
            crates/*|Cargo.toml|Cargo.lock)
                changed_rust=true
                ;;
        esac
    done < "${CAMBER_VERIFY_CHANGED_FILES}"
fi

# ---------- routing ----------
if [ ${#scope_rust[@]} -eq 0 ]; then
    if [ "${CAMBER_VERIFY_STEP:-1}" = "1" ] || [ ! -f "${CAMBER_VERIFY_CHANGED_FILES:-/nonexistent}" ] || [ "$changed_rust" = true ]; then
        # No scope: first step, unknown diff, or Rust files changed — run the
        # full workspace with the CI feature set.
        echo "[verify_step] workspace: fmt/pedant/clippy/test (no scope)"
        require_cargo
        run cargo fmt --check
        run .github/scripts/check-pedant.sh source
        run .github/scripts/check-pedant.sh tests
        run cargo clippy --workspace --features "$CAMBER_FEATURES" -- -D warnings
        run cargo test --workspace --features "$CAMBER_FEATURES"
    else
        echo "[verify_step] no Rust-relevant changes and no scope — nothing to verify"
    fi
    exit "$worst"
fi

# ---------- scoped run ----------
echo "[verify_step] scoped: ${scope_rust[*]}"
# Check formatting across the workspace, as CI does.
run cargo fmt --check
for pkg in "${scope_rust[@]}"; do
    run .github/scripts/check-pedant.sh source "$pkg"
    run .github/scripts/check-pedant.sh tests "$pkg"
done
# Branch on the featured crate instead of expanding a possibly-empty
# feature_args array — macOS /bin/bash is 3.2, where `"${empty[@]}"` under
# `set -u` aborts with "unbound variable".
for pkg in "${scope_rust[@]}"; do
    if [ "$pkg" = "$FEATURED_CRATE" ]; then
        run cargo clippy -p "$pkg" --features "$CAMBER_FEATURES" -- -D warnings
        run cargo test -p "$pkg" --features "$CAMBER_FEATURES"
    else
        run cargo clippy -p "$pkg" -- -D warnings
        run cargo test -p "$pkg"
    fi
done

exit "$worst"
