#!/usr/bin/env bash

set -euo pipefail

camber_publishable_packages() {
    printf '%s\n' camber-build camber-macros camber camber-cli
}

# One call packages the set together, so each tarball resolves its local
# siblings and not an older registry release.
camber_package_arguments() {
    local package arguments='package --locked'
    while IFS= read -r package; do
        arguments="${arguments} -p ${package}"
    done < <(camber_publishable_packages)
    printf '%s\n' "${arguments}"
}

verify_packages_main() {
    local root
    local -a arguments
    root=$(git rev-parse --show-toplevel 2>/dev/null) || return 75
    cd "${root}" || return 75
    command -v cargo >/dev/null 2>&1 || return 75
    read -r -a arguments <<<"$(camber_package_arguments)"
    cargo "${arguments[@]}"
}

[ "${CAMBER_HOOK_LIBRARY_MODE:-0}" = 1 ] || verify_packages_main "$@"
