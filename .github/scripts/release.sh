#!/usr/bin/env bash
set -euo pipefail

# `update` writes versions and changelogs. Run it through reproduce-ci.sh release
# for a local check in a disposable checkout, or use this entrypoint in CI.
ROOT=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CAMBER_SEMVER_CHECKS_BIN=$(command -v cargo-semver-checks) || {
    printf 'INFRASTRUCTURE: install cargo-semver-checks 0.50.0\n' >&2
    exit 75
}
[ "$("${CAMBER_SEMVER_CHECKS_BIN}" --version)" = 'cargo-semver-checks 0.50.0' ] || {
    printf 'ERROR: release checks require cargo-semver-checks 0.50.0\n' >&2
    exit 1
}
[ "$(release-plz --version)" = 'release-plz 0.3.169' ] || {
    printf 'ERROR: release checks require release-plz 0.3.169\n' >&2
    exit 1
}
export CAMBER_SEMVER_CHECKS_BIN
export PATH="${ROOT}/.github/scripts/release-tools:${PATH}"
exec release-plz "$@"
