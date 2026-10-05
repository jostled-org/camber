#!/usr/bin/env bash
set -euo pipefail
root=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
workflow="$root/.github/workflows/external-evidence.yml"
runner='.github/scripts/check-local-integrations.sh'
fail() { printf 'external evidence contract: %s\n' "$1" >&2; exit 1; }
if rg -q 'schedule:|cron:|compile_time|@stable|go-version: stable|image: nats:2-alpine' "$workflow"; then
    fail 'periodic runs, shared-runner measurements, or floating tool versions remain'
fi
# Local service images are pinned once, in the tool record the runner reads.
if rg -q '^\s*(services:|image:)' "$workflow"; then
    fail 'the workflow pins a service image outside .github/workflow-tools.toml'
fi
[ -x "$root/$runner" ] || fail "$runner is not executable"
for entry in '  pull_request:' '  push:' 'cargo install oha --version 1.16.0 --locked' \
    'wrk_adapter_accepts_live_output oha_adapter_accepts_live_output' \
    'Validate DNS prerequisites'; do
    rg -qF "$entry" "$workflow" || fail "missing: $entry"
done
manual="if: github.event_name == 'workflow_dispatch' && inputs.lane =="
affected="if: github.event_name != 'workflow_dispatch' || inputs.lane =="
rg -qF "$manual 'dns_public'" "$workflow" || fail 'dns_public must require manual selection'
# Print the block of job `$1`: its header through the next job header.
job_block() {
    sed -n "/^  $1:\$/,/^  [a-z_-]*:\$/p" "$workflow"
}
# The local lane jobs share one anchored step list. Its one runner step takes
# the lane from the job id, so each job runs its own lane.
anchor='    steps: &local_lane_steps'
reuse='    steps: *local_lane_steps'
[ "$(rg -cxF "$anchor" "$workflow")" = 1 ] || fail 'the local lane step list is not anchored exactly once'
anchored=$(sed -n "/^$anchor\$/,/^  [a-z_-]*:\$/p" "$workflow")
{ [ "$(printf '%s\n' "$anchored" | rg -cF "$runner")" = 1 ] \
    && printf '%s\n' "$anchored" | rg -qxF "        run: $runner \${{ github.job }}"; } \
    || fail 'the local lane steps do not run the runner once with the job lane'
steps_lines=0
for lane in nats sqs dns; do
    block=$(job_block "$lane")
    [ -n "$block" ] || fail "missing local lane job: $lane"
    selection="$affected '$lane'"
    printf '%s\n' "$block" | rg -qxF "    $selection" \
        || fail "the $lane job does not carry its own selection: $selection"
    printf '%s\n' "$block" | rg -qx "($anchor|${reuse//\*/\\*})" \
        || fail "the $lane job does not run the shared local lane steps"
    steps_lines=$((steps_lines + $(printf '%s\n' "$block" | rg -c '^    steps:')))
done
[ "$steps_lines" = 3 ] || fail 'a local lane job carries its own steps'
[ "$(rg -cxF "$reuse" "$workflow")" = 2 ] || fail 'the local lane steps are reused outside nats, sqs, and dns'
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
# The local lanes, DNS included, are dummy-credential evidence; only the public
# lane reads provider secrets. The anchored steps sit in the nats block.
for lane in nats sqs dns; do
    case "$(job_block "$lane")" in
        *CF_TOKEN*|*ACME_TEST_DOMAIN*|*secrets.*) fail "the local $lane lane reads provider secrets" ;;
    esac
done
# Every unselected lane is recorded by the runner's own writer, which needs no
# engine and no Cargo.
rg -qxF "        run: $runner --not-selected \"\${{ matrix.lane }}\"" "$workflow" \
    || fail 'the not-selected job does not record through the runner'
matrix=$(sed -n '/^  not-selected:$/,/^  [a-z_-]*:$/s/^        lane: \[\(.*\)\]$/\1/p' "$workflow")
IFS=', ' read -r -a matrix_lanes <<<"$matrix"
[ "${#matrix_lanes[@]}" -gt 0 ] || fail 'the not-selected job has no lane matrix'
scratch=$(mktemp -d "${TMPDIR:-/tmp}/camber-external-evidence.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin"
for tool in docker cargo; do
    printf '#!/bin/sh\necho "%s $*" >>"%s/effects"\nexit 1\n' "$tool" "$scratch" >"$scratch/bin/$tool"
    chmod +x "$scratch/bin/$tool"
done
record() {
    PATH="$scratch/bin:$PATH" CAMBER_EXTERNAL_EVIDENCE_DIR="$scratch/evidence" \
        CAMBER_EXTERNAL_RUN_ID=fixture-run "$root/$runner" --not-selected "$@"
}
for lane in "${matrix_lanes[@]}"; do
    record "$lane" 2>"$scratch/stderr" || { cat "$scratch/stderr" >&2; fail "cannot record unselected lane $lane"; }
    expected="{\"lane\":\"$lane\",\"run_id\":\"fixture-run\",\"status\":\"not_selected\",\"tests\":[]}"
    [ "$(cat "$scratch/evidence/$lane.json")" = "$expected" ] \
        || fail "unselected lane $lane recorded: $(cat "$scratch/evidence/$lane.json")"
done
[ "$(find "$scratch/evidence" -type f | wc -l)" -eq "${#matrix_lanes[@]}" ] \
    || fail 'recording unselected lanes wrote another file'
# An unknown lane, two lanes, or no lane is a malformed selection.
refuse() {
    local status=0
    record "$@" 2>/dev/null || status=$?
    [ "$status" = 64 ] || fail "--not-selected [$*] exited $status, not 64"
}
refuse kafka
refuse nats sqs
refuse
[ ! -e "$scratch/evidence/kafka.json" ] || fail 'an unknown lane was recorded'
[ ! -e "$scratch/effects" ] || fail "recording unselected lanes used the engine or Cargo: $(cat "$scratch/effects")"
for file in .github/workflows/ci.yml .github/scripts/reproduce-ci.sh; do
    if rg -q -- '--exclude camber-bench' "$root/$file"; then
        fail "$file excludes deterministic benchmark tests"
    fi
done
printf 'External evidence contract: PASS\n'
