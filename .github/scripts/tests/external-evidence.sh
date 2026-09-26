#!/usr/bin/env bash
set -euo pipefail
root=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
workflow="$root/.github/workflows/external-evidence.yml"
fail() { printf 'external evidence contract: %s\n' "$1" >&2; exit 1; }
if rg -q 'schedule:|cron:|compile_time|@stable|go-version: stable|image: nats:2-alpine' "$workflow"; then
    fail 'periodic runs, shared-runner measurements, or floating tool versions remain'
fi
for entry in '  pull_request:' '  push:' 'cargo install oha --version 1.16.0 --locked' \
    'wrk_adapter_accepts_live_output oha_adapter_accepts_live_output' \
    'Validate DNS prerequisites'; do
    rg -qF "$entry" "$workflow" || fail "missing: $entry"
done
rg -qF "if: github.event_name == 'workflow_dispatch' && inputs.lane == 'dns'" "$workflow" \
    || fail 'DNS must require manual selection'
preflight=$(sed -n '/      - name: Validate DNS prerequisites/,/      - uses:/p' "$workflow" \
    | sed -n '/          test /s/^          //p')
[ -n "$preflight" ] || fail 'DNS preflight is empty'
if ACME_TEST_DOMAIN='' CF_TOKEN='' bash -ec "$preflight" >/dev/null 2>&1; then
    fail 'DNS admitted missing credentials'
fi
if ACME_TEST_DOMAIN=example.invalid CF_TOKEN='' bash -ec "$preflight" >/dev/null 2>&1; then
    fail 'DNS admitted a missing token'
fi
ACME_TEST_DOMAIN=example.invalid CF_TOKEN=fixture-token bash -ec "$preflight" \
    || fail 'DNS rejected populated prerequisites'
for file in .github/workflows/ci.yml .github/scripts/reproduce-ci.sh; do
    if rg -q -- '--exclude camber-bench' "$root/$file"; then
        fail "$file excludes deterministic benchmark tests"
    fi
done
printf 'External evidence contract: PASS\n'
