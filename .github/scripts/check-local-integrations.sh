#!/usr/bin/env bash
#
# Run one local-service lane: `nats`, `sqs`, or `dns`, plus optional exact
# tests from that lane's inventory.
#
# The runner owns the lane's containers and network. It starts each service
# from the digest `.github/workflow-tools.toml` pins, on a run-scoped network,
# published only on engine-chosen loopback ports, with dummy credentials. Each
# selected test must list exactly once, then runs alone with its own cleanup
# witness. A test counts only when it passes and leaves a fresh witness that
# names this run. Teardown removes only the resources this run recorded and
# proves the engine no longer knows them.
#
# It writes `<lane>.json` for every lane into CAMBER_EXTERNAL_EVIDENCE_DIR
# (default `logs/external`). The selected lane reads `passed`, `failed`, or
# `infrastructure_unavailable`; the other lanes read `not_selected`.
#
# `--not-selected <lane>` writes only that lane's `not_selected` record, for
# any external evidence lane the workflow does not select. It needs no engine
# and no Cargo.
#
# Exit 0 means the lane passed, 64 a malformed selection, and 75 unavailable
# infrastructure (no engine, no Cargo, or an unpinned toolchain). Any other
# status is a failed lane.

set -euo pipefail

RUNNER_ROOT=$(CDPATH='' cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
readonly RUNNER_ROOT
# shellcheck source=.github/scripts/reproduce-ci.sh
CAMBER_HOOK_LIBRARY_MODE=1 source "${RUNNER_ROOT}/.github/scripts/reproduce-ci.sh"

readonly LOCAL_TEST_PACKAGE='camber'
readonly LOCAL_TEST_ROOT='external_feature_services'
readonly LOCAL_ENGINE='docker'
readonly READINESS_BOUND_SECONDS=60
readonly LOG_TAIL_LINES=200
# Labels matching the Rust fixture owners in tests/support/local_integrations.
readonly SERVICE_RUN_LABEL='camber.local.run'
readonly SERVICE_NAME_LABEL='camber.local.service'
readonly MAX_RUN_ID_BYTES=64
readonly STATUS_USAGE=64
# Dummy credentials the local services accept. They name no real account.
readonly LOCAL_ACCESS_KEY='camber-local'
readonly LOCAL_SECRET_KEY='camber-local-secret'
readonly LOCAL_REGION='us-east-1'
# Real provider credentials and proxies never reach a selected test: it talks
# only to this run's loopback services.
readonly SCRUBBED_ENVIRONMENT=(
    CF_TOKEN ACME_TEST_DOMAIN AWS_PROFILE AWS_SESSION_TOKEN
    HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy
)

# Run state. `owned_containers` and `owned_network` are recorded before the
# engine creates them, so an ambiguous create is still reaped.
lane=''
run_id=''
evidence_dir=''
witness_dir=''
owned_network=''
owned_containers=()
# The run's own directory: mounted service configuration and copied trust
# roots. Teardown removes it.
owned_root=''
selected_tests=()
# `service reference` lines for the lane, resolved once from the tool record.
pinned_images=''
test_environment=()
torn_down=0
disk_before='null'
disk_after='null'
disk_started=0

local_lanes() {
    printf '%s\n' nats sqs dns
}

# Every external evidence lane. Only the local lanes run here; the others run
# in their own workflow jobs and use this runner to record `not_selected`.
external_lanes() {
    local_lanes
    printf '%s\n' dns_public docker go load_generators
}

# The exact tests each lane admits. A lane gains a test only when the test
# that owns its local behavior lands.
lane_tests() {
    case "$1" in
        nats)
            printf '%s\n' \
                external_nats::nats_local_contract_matrix \
                external_nats::nats_local_jetstream_publish_matrix
            ;;
        sqs)
            printf '%s\n' external_sqs::sqs_local_standard_queue_contract_matrix
            ;;
        dns)
            printf '%s\n' \
                local_dns01::dns_local_acme_multizone_issuance_and_cleanup \
                local_dns01::dns_local_startup_renewal_and_cache_restart \
                local_dns01::dns_local_cancelled_order_keeps_unrelated_txt
            ;;
    esac
}

lane_features() {
    case "$1" in
        nats) printf 'nats\n' ;;
        sqs) printf 'sqs\n' ;;
        dns) printf 'dns01\n' ;;
    esac
}

# Services in start order; a service starts after the services it resolves.
lane_services() {
    case "$1" in
        nats) printf '%s\n' nats ;;
        sqs) printf '%s\n' elasticmq ;;
        dns) printf '%s\n' challtestsrv pebble ;;
    esac
}

# Set the caller-visible `spec_*` values for `service`.
#
# `spec_ports` lists `name:port/protocol` publications. `spec_health` is an
# in-container protocol probe the engine runs; the runner waits for the engine
# to report it healthy. Pebble and challtestsrv ship without a shell, so the
# selected test proves their readiness through the Rust fixture probes.
# `spec_trust` names the root certificate inside the image that the service's
# own HTTPS listeners present; the runner copies it out for the test to trust.
# `spec_config` is a `file:container-path` the runner writes from
# `service_config` into the run's own directory and copies into the container
# before it starts.
#
# NATS runs JetStream with its store inside the container, so the run's
# container removal also removes every stored message. `/healthz` reports
# healthy only once JetStream is ready.
service_spec() {
    spec_ports=()
    spec_command=()
    spec_environment=()
    spec_health=''
    spec_trust=''
    spec_config=''
    case "$1" in
        nats)
            spec_ports=(client:4222/tcp)
            spec_command=(nats-server -p 4222 -m 8222 -js -sd /tmp/nats/jetstream)
            spec_health='wget -q -O /dev/null http://127.0.0.1:8222/healthz'
            ;;
        elasticmq)
            spec_ports=(query:9324/tcp)
            spec_health="wget -q -O /dev/null --post-data 'Action=ListQueues&Version=2012-11-05' http://127.0.0.1:9324/"
            ;;
        challtestsrv)
            # DNS is published over TCP: not every engine forwards a published
            # UDP port. Pebble resolves over the run network, not this port.
            spec_ports=(management:8055/tcp dns:8053/tcp)
            spec_command=(
                -management :8055 -dnsserver :8053 -http01 '' -https01 ''
                -tlsalpn01 '' -doh '' -defaultIPv6 '' -defaultIPv4 127.0.0.1
            )
            ;;
        pebble)
            spec_ports=(acme:14000/tcp management:15000/tcp)
            spec_config='pebble-config.json:/test/config/camber-pebble.json'
            spec_command=(
                -config /test/config/camber-pebble.json -strict
                -dnsserver challtestsrv:8053
            )
            # Fresh accounts prove DNS challenges. Renewals always reuse valid
            # authorizations; neither reuse nor nonce refusal is random.
            spec_environment=(
                PEBBLE_VA_NOSLEEP=1 PEBBLE_WFE_NONCEREJECT=0 PEBBLE_AUTHZREUSE=100
            )
            spec_trust='/test/certs/pebble.minica.pem'
            ;;
    esac
}

# The configuration file `service_spec` names for `service`.
#
# Pebble picks a random profile for an order that names none, so its one
# profile fixes every leaf's validity: 90 days, outside the 30-day renewal
# window. Every other value is the image's own default configuration.
service_config() {
    case "$1" in
        pebble)
            cat <<'JSON'
{
  "pebble": {
    "listenAddress": "0.0.0.0:14000",
    "managementListenAddress": "0.0.0.0:15000",
    "certificate": "test/certs/localhost/cert.pem",
    "privateKey": "test/certs/localhost/key.pem",
    "httpPort": 5002,
    "tlsPort": 5001,
    "ocspResponderURL": "",
    "externalAccountBindingRequired": false,
    "retryAfter": { "authz": 3, "order": 5 },
    "keyAlgorithm": "ecdsa",
    "profiles": {
      "default": { "description": "Camber local lane", "validityPeriod": 7776000 }
    }
  }
}
JSON
            ;;
    esac
}

usage() {
    printf 'Usage: %s <nats|sqs|dns> [exact-test...]\n' "$0" >&2
    printf '       %s --not-selected <lane>\n' "$0" >&2
}

report() {
    printf 'local lane %s: %s\n' "${lane:-?}" "$1" >&2
}

cargo_test() {
    cargo test -p "${LOCAL_TEST_PACKAGE}" \
        --features "$(lane_features "${lane}")" --test "${LOCAL_TEST_ROOT}" "$@"
}

engine() {
    "${LOCAL_ENGINE}" "$@"
}

# --- Selection -----------------------------------------------------------

valid_run_id() {
    local pattern='^[A-Za-z0-9_-]+$'
    [ "${#1}" -le "${MAX_RUN_ID_BYTES}" ] && [[ $1 =~ ${pattern} ]]
}

# Admit the lane and its exact tests before any engine or Cargo effect.
admit_selection() {
    local requested test
    [ "$#" -gt 0 ] || { usage; report 'no lane was selected'; return "${STATUS_USAGE}"; }
    listed "$1" local_lanes || {
        usage
        printf 'local lane runner: unknown lane: %s\n' "$1" >&2
        return "${STATUS_USAGE}"
    }
    lane="$1"
    shift
    for requested in "$@"; do
        listed "${requested}" lane_tests "${lane}" || {
            report "unknown exact test: ${requested}"
            return "${STATUS_USAGE}"
        }
        for test in "${selected_tests[@]+"${selected_tests[@]}"}"; do
            [ "${test}" != "${requested}" ] || {
                report "test selected twice: ${requested}"
                return "${STATUS_USAGE}"
            }
        done
        selected_tests+=("${requested}")
    done
    [ "${#selected_tests[@]}" -gt 0 ] || while IFS= read -r test; do
        selected_tests+=("${test}")
    done < <(lane_tests "${lane}")
    [ "${#selected_tests[@]}" -gt 0 ] || {
        report 'no exact test is registered for this lane'
        return 1
    }
}

admit_run() {
    run_id="${CAMBER_EXTERNAL_RUN_ID:-local-$(date +%s)-$$}"
    valid_run_id "${run_id}" || {
        report "CAMBER_EXTERNAL_RUN_ID must be 1-${MAX_RUN_ID_BYTES} ASCII letters, digits, '-', or '_'"
        return "${STATUS_USAGE}"
    }
    evidence_dir="${CAMBER_EXTERNAL_EVIDENCE_DIR:-${RUNNER_ROOT}/logs/external}"
    mkdir -p "${evidence_dir}" || return "${STATUS_INFRASTRUCTURE}"
    evidence_dir=$(CDPATH='' cd "${evidence_dir}" && pwd) || return "${STATUS_INFRASTRUCTURE}"
}

# --- Evidence ------------------------------------------------------------

json_string_array() {
    local item separator=''
    printf '['
    for item in "$@"; do
        printf '%s%s' "${separator}" "$(json_string "${item}")"
        separator=','
    done
    printf ']'
}

# Write the evidence of lane `each`: the selected lane's result under exit
# status `status`, or `not_selected` for any other lane.
write_lane_evidence() {
    local each="$1" status="${2-}" state
    case "${each}" in
        "${lane}")
            state=$(evidence_status_name "${status}")
            printf '{"lane":"%s","run_id":"%s","status":"%s","tests":%s,"disk":{"before":%s,"after":%s}}\n' \
                "${each}" "${run_id}" "${state}" \
                "$(json_string_array "${selected_tests[@]+"${selected_tests[@]}"}")" \
                "${disk_before}" "${disk_after}"
            ;;
        *)
            printf '{"lane":"%s","run_id":"%s","status":"not_selected","tests":[]}\n' \
                "${each}" "${run_id}"
            ;;
    esac >"${evidence_dir}/${each}.json"
}

write_evidence() {
    local status="$1" each
    for each in $(local_lanes); do
        write_lane_evidence "${each}" "${status}" || return 1
    done
}

# Record `not_selected` for one external lane, with no engine or Cargo effect.
record_not_selected() {
    { [ "$#" -eq 1 ] && listed "$1" external_lanes; } || {
        usage
        printf 'local lane runner: --not-selected takes one external lane\n' >&2
        return "${STATUS_USAGE}"
    }
    admit_run || return $?
    write_lane_evidence "$1" || {
        report "cannot write $1 evidence under ${evidence_dir}"
        return 1
    }
}

# Engine totals include other runs. Image sizes include shared layers and are
# not additive. Cargo target usage is separate, in KiB, from service images.
disk_snapshot() {
    local images="$1" report service reference by_digest cached size separator='' kib
    report=$(engine system df --format '{{json .}}') || return 1
    [ -n "${report}" ] || return 1
    kib=$(directory_kib "${CARGO_TARGET_DIR:-${RUNNER_ROOT}/target}") || return 1
    printf '{"engine_report":%s,"cargo_target_kib":%s,"images":[' "$(json_string "${report}")" "${kib}"
    while read -r service reference; do
        by_digest=$(digest_reference "${reference}")
        cached=$(engine image ls -q "${by_digest}") || return 1
        size='null'
        if [ -n "${cached}" ]; then
            size=$(engine image inspect --format '{{.Size}}' "${by_digest}") || return 1
            [[ ${size} =~ ^[0-9]+$ ]] || return 1
        fi
        printf '%s{"service":%s,"reference":%s,"size_bytes":%s}' \
            "${separator}" "$(json_string "${service}")" "$(json_string "${reference}")" "${size}"
        separator=','
    done <<<"${images}"
    printf ']}\n'
}

# --- Admission of tools, images, and tests -------------------------------

require_engine() {
    command -v "${LOCAL_ENGINE}" >/dev/null 2>&1 || {
        report "container engine ${LOCAL_ENGINE} is not on PATH"
        return "${STATUS_INFRASTRUCTURE}"
    }
    local answer
    answer=$(engine info 2>&1 >/dev/null) || {
        report "container engine ${LOCAL_ENGINE} does not answer: ${answer}"
        return "${STATUS_INFRASTRUCTURE}"
    }
}

# Print `service reference` for every lane service, from the one inventory.
lane_images() {
    local service reference
    for service in $(lane_services "${lane}"); do
        reference=$(pinned_service_image "${RUNNER_ROOT}" "${service}") || return 1
        printf '%s %s\n' "${service}" "${reference}"
    done
}

# The repository and digest of a pinned reference, without its tag.
digest_reference() {
    local reference="$1" repository
    repository=${reference%@*}
    printf '%s@%s\n' "${repository%:*}" "${reference#*@}"
}

# Pull each `service reference` in `images` by digest and refuse an engine
# that reports another one.
verify_images() {
    local images="$1" service reference by_digest identities
    while read -r service reference; do
        by_digest=$(digest_reference "${reference}")
        engine pull --quiet "${by_digest}" >/dev/null || {
            report "cannot pull ${service} image ${by_digest}"
            return 1
        }
        identities=$(engine image inspect \
            --format '{{range .RepoDigests}}{{println .}}{{end}}' "${by_digest}") || {
            report "cannot inspect ${service} image ${by_digest}"
            return 1
        }
        case $'\n'"${identities}"$'\n' in
            *"@${reference#*@}"$'\n'*) ;;
            *)
                report "engine reports ${service} as ${identities//$'\n'/ }, not ${reference#*@}"
                return 1
                ;;
        esac
    done <<<"${images}"
}

# Each selected test lists exactly once under --exact --ignored.
verify_listings() {
    local test problem
    for test in "${selected_tests[@]}"; do
        problem=$(listing_problem "${test}" "${test}" \
            cargo_test -- "${test}" --exact --ignored --list)
        [ -z "${problem}" ] || { report "${problem}"; return 1; }
    done
}

# --- Owned services ------------------------------------------------------

container_name() {
    printf 'camber-local-%s-%s-%s\n' "${run_id}" "${lane}" "$1"
}

start_service() {
    local service="$1" reference="$2" name port variable arguments=()
    service_spec "${service}"
    name=$(container_name "${service}")
    arguments=(
        --pull never --name "${name}" --network "${owned_network}"
        --network-alias "${service}"
        --label "${SERVICE_RUN_LABEL}=${run_id}" --label "${SERVICE_NAME_LABEL}=${service}"
    )
    for port in "${spec_ports[@]}"; do
        arguments+=(-p "127.0.0.1::${port#*:}")
    done
    for variable in "${spec_environment[@]+"${spec_environment[@]}"}"; do
        arguments+=(-e "${variable}")
    done
    [ -z "${spec_health}" ] || arguments+=(
        --health-cmd "${spec_health}" --health-interval 1s
        --health-timeout 5s --health-retries "${READINESS_BOUND_SECONDS}"
    )
    arguments+=("$(digest_reference "${reference}")")
    arguments+=("${spec_command[@]+"${spec_command[@]}"}")
    owned_containers+=("${name}")
    case "${spec_config}" in
        '') engine run -d "${arguments[@]}" >/dev/null ;;
        *) start_configured "${service}" "${name}" "${arguments[@]}" ;;
    esac || {
        report "cannot start ${service}"
        return 1
    }
}

# Create the container, copy its configuration in, then start it. A copy
# reaches any engine; a host mount reaches only the paths its host shares.
start_configured() {
    local service="$1" name="$2" file
    shift 2
    file="${owned_root}/${spec_config%%:*}"
    service_config "${service}" >"${file}" || return 1
    engine create "$@" >/dev/null || return 1
    engine cp "${file}" "${name}:${spec_config#*:}" >/dev/null || return 1
    engine start "${name}" >/dev/null
}

# Wait, inside one bound, for the engine to report the service's protocol
# probe healthy.
await_health() {
    local service="$1" name state='' waited=0
    service_spec "${service}"
    [ -n "${spec_health}" ] || return 0
    name=$(container_name "${service}")
    while [ "${waited}" -lt "${READINESS_BOUND_SECONDS}" ]; do
        state=$(engine inspect --format '{{.State.Health.Status}}' "${name}") || state='absent'
        case "${state}" in
            healthy) return 0 ;;
            unhealthy|absent) break ;;
        esac
        sleep 1
        waited=$((waited + 1))
    done
    report "${service} did not report a healthy protocol probe (${state})"
    return 1
}

# The engine-chosen loopback address for one container port.
published_address() {
    local name="$1" port="$2" line
    while IFS= read -r line; do
        case "${line}" in
            127.0.0.1:[0-9]*) printf '%s\n' "${line}"; return 0 ;;
        esac
    done < <(engine port "${name}" "${port}")
    report "${name} published no loopback address for ${port}"
    return 1
}

# An environment-variable fragment: `challtestsrv` reads `CHALLTESTSRV`.
upper() {
    printf '%s' "$1" | tr 'abcdefghijklmnopqrstuvwxyz-' 'ABCDEFGHIJKLMNOPQRSTUVWXYZ_'
}

# Record the environment each selected test reads its endpoints from.
publish_endpoints() {
    local service="$1" name port address
    service_spec "${service}"
    name=$(container_name "${service}")
    for port in "${spec_ports[@]}"; do
        address=$(published_address "${name}" "${port#*:}") || return 1
        test_environment+=("CAMBER_LOCAL_$(upper "${service}")_$(upper "${port%%:*}")=${address}")
        case "${service}" in
            nats) test_environment+=("NATS_URL=nats://${address}") ;;
        esac
    done
    publish_trust "${service}"
}

# Copy the root the service's HTTPS listeners present out of its container,
# into the run's own directory, and record where the test reads it.
publish_trust() {
    local service="$1" destination
    service_spec "${service}"
    [ -n "${spec_trust}" ] || return 0
    destination="${owned_root}/${service}-trust.pem"
    engine cp "$(container_name "${service}"):${spec_trust}" "${destination}" >/dev/null || {
        report "cannot copy the ${service} trust root"
        return 1
    }
    test_environment+=("CAMBER_LOCAL_$(upper "${service}")_TRUST=${destination}")
}

# Start each `service reference` in `images` on the run's own network.
start_services() {
    local images="$1" service reference
    owned_root=$(mktemp -d "${evidence_dir}/${lane}-run.XXXXXX") || {
        report 'cannot create the run directory'
        return 1
    }
    owned_network="camber-local-${run_id}-${lane}"
    engine network create --label "${SERVICE_RUN_LABEL}=${run_id}" "${owned_network}" \
        >/dev/null || { report 'cannot create the lane network'; return 1; }
    while read -r service reference; do
        start_service "${service}" "${reference}" || return 1
    done <<<"${images}"
    for service in $(lane_services "${lane}"); do
        await_health "${service}" || return 1
        publish_endpoints "${service}" || return 1
    done
    test_environment+=(
        "AWS_ACCESS_KEY_ID=${LOCAL_ACCESS_KEY}"
        "AWS_SECRET_ACCESS_KEY=${LOCAL_SECRET_KEY}"
        "AWS_REGION=${LOCAL_REGION}"
    )
}

# Remove one resource, then prove the engine no longer knows it.
reap() {
    local kind="$1" name="$2" output
    case "${kind}" in
        container) engine rm -f -v "${name}" >/dev/null 2>&1 || true ;;
        network) engine network rm "${name}" >/dev/null 2>&1 || true ;;
    esac
    if output=$(engine "${kind}" inspect "${name}" 2>&1); then
        report "${kind} ${name} survived removal"
        return 1
    fi
    case "$(printf '%s' "${output}" | tr '[:upper:]' '[:lower:]')" in
        *'no such'*|*'not found'*) ;;
        *)
            report "cannot prove ${kind} ${name} is gone: ${output}"
            return 1
            ;;
    esac
}

capture_logs() {
    local name
    for name in "${owned_containers[@]+"${owned_containers[@]}"}"; do
        engine logs --tail "${LOG_TAIL_LINES}" "${name}" \
            >"${evidence_dir}/${lane}-${name}.log" 2>&1 \
            || report "cannot capture the logs of ${name}"
    done
}

# Remove every recorded resource once. Nonzero means residue survived.
teardown() {
    local name status=0
    [ "${torn_down}" = 0 ] || return 0
    torn_down=1
    capture_logs
    for name in "${owned_containers[@]+"${owned_containers[@]}"}"; do
        reap container "${name}" || status=1
    done
    [ -z "${owned_network}" ] || reap network "${owned_network}" || status=1
    if [ -n "${owned_root}" ]; then
        rm -rf "${owned_root}"
        [ ! -e "${owned_root}" ] || { report "${owned_root} survived removal"; status=1; }
    fi
    return "${status}"
}

# --- Selected tests ------------------------------------------------------

# Succeed when `witness` is a completed cleanup record naming this run.
fresh_witness() {
    local witness="$1" document
    local run_pattern="\"run_id\"[[:space:]]*:[[:space:]]*\"${run_id}\""
    local status_pattern='"cleanup_status"[[:space:]]*:[[:space:]]*"completed"'
    local resources_pattern='"resources"[[:space:]]*:[[:space:]]*\[[[:space:]]*"'
    [ -f "${witness}" ] || { report "no cleanup witness at ${witness}"; return 1; }
    document=$(<"${witness}")
    [[ ${document} =~ ${run_pattern} ]] \
        || { report "cleanup witness names another run: ${document}"; return 1; }
    [[ ${document} =~ ${status_pattern} ]] \
        || { report "cleanup witness is not completed: ${document}"; return 1; }
    [[ ${document} =~ ${resources_pattern} ]] \
        || { report "cleanup witness names no resource: ${document}"; return 1; }
}

run_selected_test() {
    local test="$1" index="$2" witness output status=0
    witness="${witness_dir}/${index}-${test//::/-}.json"
    output=$(
        unset "${SCRUBBED_ENVIRONMENT[@]}"
        [ "${#test_environment[@]}" -eq 0 ] || export "${test_environment[@]}"
        export CAMBER_EXTERNAL_RUN_ID="${run_id}" CAMBER_EXTERNAL_CLEANUP_WITNESS="${witness}"
        cargo_test -- "${test}" --exact --ignored --test-threads=1 2>&1
    ) || status=$?
    printf '%s\n' "${output}" | tee -a "${evidence_dir}/${lane}.log" || {
        report "cannot append to ${evidence_dir}/${lane}.log"
        return 1
    }
    [ "${status}" -eq 0 ] || { report "${test} failed with status ${status}"; return 1; }
    ran_one_passing_test "${output}" || {
        report "${test} did not run exactly one passing test"
        return 1
    }
    fresh_witness "${witness}"
}

run_selected_tests() {
    local test index=0 status=0
    witness_dir=$(mktemp -d "${evidence_dir}/${lane}-witness.XXXXXX") || return 1
    for test in "${selected_tests[@]}"; do
        index=$((index + 1))
        run_selected_test "${test}" "${index}" || status=1
    done
    return "${status}"
}

# --- Lane ----------------------------------------------------------------

run_lane() {
    require_engine || return $?
    require_workflow_tools "${RUNNER_ROOT}" cargo || return $?
    pinned_images=$(lane_images) || return 1
    disk_before=$(disk_snapshot "${pinned_images}") || {
        disk_before='null'
        report 'cannot record initial disk use'
        return 1
    }
    disk_started=1
    cargo_test --no-run || { report 'the test root did not build'; return 1; }
    verify_listings || return 1
    verify_images "${pinned_images}" || return 1
    start_services "${pinned_images}" || return 1
    run_selected_tests
}

finish() {
    local status="$1" cleanup=0
    teardown || cleanup=$?
    [ "${cleanup}" -eq 0 ] || [ "${status}" -ne 0 ] || status=1
    if [ "${disk_started}" -eq 1 ]; then
        disk_after=$(disk_snapshot "${pinned_images}") || {
            disk_after='null'
            report 'cannot record final disk use'
            [ "${status}" -ne 0 ] || status=1
        }
    fi
    write_evidence "${status}" || {
        report "cannot write lane evidence under ${evidence_dir}"
        [ "${status}" -ne 0 ] || status=1
    }
    report "$(evidence_status_name "${status}")"
    return "${status}"
}

main() {
    local status=0
    case "${1-}" in
        --not-selected)
            shift
            record_not_selected "$@" || return $?
            return 0
            ;;
    esac
    admit_selection "$@" || status=$?
    case "${status}:${lane}" in
        0:*) ;;
        *:) return "${status}" ;;
        *)
            # A refused selection still records evidence when the run admits.
            if admit_run; then
                write_evidence "${status}" \
                    || report "cannot write lane evidence under ${evidence_dir}"
            fi
            return "${status}"
            ;;
    esac
    admit_run || return $?
    trap 'teardown || true' EXIT
    trap 'teardown || true; exit 130' INT TERM
    run_lane || status=$?
    finish "${status}"
}

main "$@"
