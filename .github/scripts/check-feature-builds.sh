#!/usr/bin/env bash
#
# Check the camber library and the focused_api_contracts root, which holds
# the documented integration examples, with no optional feature, with each
# optional integration feature alone, and with the whole workflow feature set.
# A feature or an example that compiles only beside another fails here.

set -euo pipefail

ROOT=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck source=.github/scripts/reproduce-ci.sh
CAMBER_HOOK_LIBRARY_MODE=1 source "${ROOT}/.github/scripts/reproduce-ci.sh"

# Each optional integration feature the library must build with alone.
camber_isolated_features() {
    printf '%s\n' nats sqs dns01 grpc
}

check_feature_builds() {
    local features status
    cd "${ROOT}"
    for features in '' $(camber_isolated_features) "${CAMBER_WORKFLOW_FEATURES}"; do
        printf '==> feature build [%s]\n' "${features}"
        status=0
        cargo check -p camber --lib --test focused_api_contracts --no-default-features \
            --features "${features}" \
            || status=$?
        [ "${status}" -eq 0 ] || {
            printf 'ERROR: feature build [%s] failed with status %s\n' \
                "${features}" "${status}" >&2
            return "${status}"
        }
    done
}

[ "${CAMBER_HOOK_LIBRARY_MODE:-0}" = 1 ] || check_feature_builds
