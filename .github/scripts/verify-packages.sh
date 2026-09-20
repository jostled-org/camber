#!/usr/bin/env bash

set -euo pipefail

camber_publishable_packages() {
    printf '%s\n' camber-build camber-macros camber camber-cli
}

verify_packages_main() {
    local package root
    root=$(git rev-parse --show-toplevel 2>/dev/null) || return 75
    cd "${root}" || return 75
    command -v cargo >/dev/null 2>&1 || return 75
    while IFS= read -r package; do
        cargo package --locked -p "${package}" || return $?
    done < <(camber_publishable_packages)
}

[ "${CAMBER_HOOK_LIBRARY_MODE:-0}" = 1 ] || verify_packages_main "$@"
