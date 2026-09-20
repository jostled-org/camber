#!/usr/bin/env bash

set -euo pipefail

camber_pedant_packages() {
    printf '%s\n' camber camber-bench camber-build camber-cli camber-macros
}

pedant_tree() {
    case "$1" in
        source) printf 'src\n' ;;
        tests) printf 'tests\n' ;;
        *) return 64 ;;
    esac
}

known_package() {
    local expected="$1" package
    while IFS= read -r package; do
        [ "${package}" = "${expected}" ] && return 0
    done < <(camber_pedant_packages)
    return 1
}

check_package_tree() {
    local mode="$1" package="$2" tree file
    local -a files=()
    tree=$(pedant_tree "${mode}") || return 64
    while IFS= read -r file; do
        files+=("${file}")
    done < <(git ls-files "crates/${package}/${tree}")
    [ "${#files[@]}" -gt 0 ] || {
        printf 'ERROR: no tracked %s files for %s\n' "${mode}" "${package}" >&2
        return 1
    }
    pedant check --format github "${files[@]}"
}

check_pedant_main() {
    local mode="${1:-}" requested_package="${2:-}" package root
    root=$(git rev-parse --show-toplevel 2>/dev/null) || return 75
    cd "${root}" || return 75
    command -v pedant >/dev/null 2>&1 || return 75
    pedant_tree "${mode}" >/dev/null || {
        printf 'Usage: %s <source|tests> [package]\n' "$0" >&2
        return 64
    }
    if [ -n "${requested_package}" ]; then
        known_package "${requested_package}" || {
            printf 'ERROR: unknown package: %s\n' "${requested_package}" >&2
            return 64
        }
        check_package_tree "${mode}" "${requested_package}"
        return
    fi
    while IFS= read -r package; do
        check_package_tree "${mode}" "${package}" || return $?
    done < <(camber_pedant_packages)
}

[ "${CAMBER_HOOK_LIBRARY_MODE:-0}" = 1 ] || check_pedant_main "$@"
