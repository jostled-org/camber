//! A scripted `docker` whose whole state lives in a directory the test owns.
//!
//! It models only what the fixture and the lane runner reach: networks and
//! containers with labels, label and name filters, removal, published loopback
//! ports, logs, and image identity. Every container it starts is backed by a
//! real detached process, so "the container is gone" is checked against a
//! process the operating system reports, not only a state file.
//!
//! Image identity answers with the repository digest the reference names, or
//! with [`SUBSTITUTED_DIGEST`] when the stub is told to lie. A bare tag names
//! no digest, so it always answers with the substituted identity: a floating
//! reference can never prove it is the pinned image.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::service::SERVICE_LABEL;
use crate::delivery_fixture::{quoted, run_bounded, write_executable, write_file};
use crate::temp_support::TempRoot;

/// Images under this prefix are absent: `run` refuses them.
pub const MISSING_IMAGE_PREFIX: &str = "camber.invalid/";

/// The identity a lying engine reports for every image.
pub const SUBSTITUTED_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000001";

/// The loopback address `port` reports for a service with no peer.
pub const UNPUBLISHED_ENDPOINT: &str = "127.0.0.1:49152";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageIdentity {
    Pinned,
    Substituted,
}

/// One container the stub started, as its history records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartedContainer {
    pub name: Box<str>,
    pub process: u32,
    pub image: Box<str>,
    pub network: Box<str>,
    pub publications: Box<[Box<str>]>,
}

pub struct EngineStub {
    root: TempRoot,
}

impl EngineStub {
    pub fn new() -> Self {
        let root = TempRoot::new().expect("engine stub root was not created");
        let stub = Self { root };
        for directory in [
            "bin",
            "state/containers",
            "state/networks",
            "state/endpoints",
            "state/image-files",
        ] {
            fs::create_dir_all(stub.root.path().join(directory))
                .expect("engine stub directory was not created");
        }
        stub.set_identity(ImageIdentity::Pinned);
        let script = ENGINE_SCRIPT
            .replace("@STATE@", &quoted(&stub.state()))
            .replace("@SUBSTITUTE@", SUBSTITUTED_DIGEST)
            .replace("@MISSING@", MISSING_IMAGE_PREFIX)
            .replace("@UNPUBLISHED@", UNPUBLISHED_ENDPOINT)
            .replace("@SERVICE_LABEL@", SERVICE_LABEL);
        write_executable(&stub.program(), script);
        stub
    }

    /// The directory holding only the stub `docker`.
    pub fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    pub fn program(&self) -> PathBuf {
        self.bin().join("docker")
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    pub fn set_identity(&self, identity: ImageIdentity) {
        let answer: &[u8] = match identity {
            ImageIdentity::Pinned => b"pinned\n",
            ImageIdentity::Substituted => b"substituted\n",
        };
        write_file(&self.state().join("identity"), answer);
    }

    /// Seed a file copied from each controlled image into its containers.
    pub fn seed_image_file(&self, path: &str, bytes: &[u8]) {
        write_file(
            &self
                .state()
                .join("image-files")
                .join(path.trim_start_matches('/')),
            bytes,
        );
    }

    /// Refuse one disk measurement while leaving service commands available.
    pub fn fail_disk_sample(&self, sample: usize) {
        write_file(
            &self.state().join("fail-disk-sample"),
            sample.to_string().as_bytes(),
        );
    }

    /// Answer `port` for containers labelled as `service` with `address`.
    pub fn publish_endpoint(&self, service: &str, address: SocketAddr) {
        write_file(
            &self.state().join("endpoints").join(service),
            format!("{address}\n").as_bytes(),
        );
    }

    /// A container and a network no fixture owns, which no teardown may
    /// touch. Returns the container's backing process.
    pub fn seed_foreign(&self, container: &str, network: &str) -> u32 {
        self.seed(
            &["network", "create", "--label", "owner=foreign", network],
            "foreign network",
        );
        self.seed(
            &[
                "run",
                "-d",
                "--name",
                container,
                "--network",
                network,
                "--label",
                "owner=foreign",
                "foreign.invalid/service@sha256:ffff",
            ],
            "foreign container",
        );
        self.process_of(container)
            .expect("foreign container has a backing process")
    }

    /// Run one engine command that must succeed in seeding `what`.
    fn seed(&self, args: &[&str], what: &str) {
        let mut command = Command::new(self.program());
        command.args(args);
        assert_eq!(run_bounded(command).status, 0, "{what} was not seeded");
    }

    /// The containers the engine currently knows.
    pub fn containers(&self) -> Box<[Box<str>]> {
        entries(&self.state().join("containers"))
    }

    /// The networks the engine currently knows.
    pub fn networks(&self) -> Box<[Box<str>]> {
        entries(&self.state().join("networks"))
    }

    /// The backing process of `container`; `None` only when the engine
    /// records no process for it.
    ///
    /// # Panics
    ///
    /// When the recorded process cannot be read or parsed: reading it as
    /// absent would leave that process running past [`Self::close`].
    pub fn process_of(&self, container: &str) -> Option<u32> {
        let path = self.state().join("containers").join(container).join("pid");
        match fs::read_to_string(&path) {
            Ok(pid) => {
                Some(pid.trim().parse().unwrap_or_else(|error| {
                    panic!("{} holds no process ID: {error}", path.display())
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("cannot read {}: {error}", path.display()),
        }
    }

    /// Every container ever started, in start order, including removed ones.
    ///
    /// # Panics
    ///
    /// When a history line is malformed: skipping it would hide a start.
    pub fn started(&self) -> Box<[StartedContainer]> {
        read_log(&self.state().join("started.log"))
            .lines()
            .map(|line| {
                started_container(line)
                    .unwrap_or_else(|| panic!("started.log holds a malformed line: {line:?}"))
            })
            .collect()
    }

    /// Every image reference `run` was asked for, refused ones included.
    pub fn requested_images(&self) -> Box<[Box<str>]> {
        read_log(&self.state().join("images.log"))
            .lines()
            .map(Box::from)
            .collect()
    }

    /// Every engine invocation, one argument vector per line.
    pub fn calls(&self) -> Box<str> {
        read_log(&self.state().join("calls.log"))
    }

    /// Stop every backing process still running, then remove the stub.
    ///
    /// # Panics
    ///
    /// When `kill` fails on a backing process that is still running.
    pub fn close(self) {
        for process in self
            .containers()
            .iter()
            .filter_map(|container| self.process_of(container))
        {
            let mut command = Command::new("kill");
            command.arg(process.to_string());
            let run = run_bounded(command);
            assert!(
                run.status == 0 || !process_alive(process),
                "backing process {process} survived kill: {}",
                run.output
            );
        }
        self.root.close().expect("engine stub root was not removed");
    }
}

impl Default for EngineStub {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `process` is still running: absent and zombie both read as gone.
pub fn process_alive(process: u32) -> bool {
    let mut command = Command::new("ps");
    command.args(["-o", "stat=", "-p", &process.to_string()]);
    let run = run_bounded(command);
    let state = run.output.trim();
    run.status == 0 && !state.is_empty() && !state.starts_with('Z')
}

/// One `started.log` line: name, process, image, network, and publications,
/// tab-separated; `None` when a field is missing or the process is no number.
fn started_container(line: &str) -> Option<StartedContainer> {
    let mut fields = line.split('\t');
    let name = fields.next()?.into();
    let process = fields.next()?.parse().ok()?;
    let image = fields.next()?.into();
    let network = fields.next()?.into();
    let publications = fields
        .next()?
        .split(' ')
        .filter(|publication| !publication.is_empty())
        .map(Box::from)
        .collect();
    Some(StartedContainer {
        name,
        process,
        image,
        network,
        publications,
    })
}

fn entries(directory: &Path) -> Box<[Box<str>]> {
    let mut names: Vec<Box<str>> = fs::read_dir(directory)
        .expect("engine stub state was not readable")
        .map(|entry| {
            entry
                .expect("engine stub entry was not readable")
                .file_name()
                .to_string_lossy()
                .into()
        })
        .collect();
    names.sort_unstable();
    names.into_boxed_slice()
}

fn read_log(path: &Path) -> Box<str> {
    match fs::read_to_string(path) {
        Ok(text) => text.into_boxed_str(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Box::default(),
        Err(error) => panic!("cannot read {}: {error}", path.display()),
    }
}

const ENGINE_SCRIPT: &str = r#"#!/usr/bin/env bash
set -u
state=@STATE@
printf '%s\n' "$*" >>"${state}/calls.log"

fail() {
    printf 'Error response from daemon: %s\n' "$1" >&2
    exit "${2:-1}"
}

last_operand() {
    local argument operand=''
    for argument in "$@"; do
        case "${argument}" in
            -*) ;;
            *) operand="${argument}" ;;
        esac
    done
    printf '%s' "${operand}"
}

resolve() {
    printf '%s' "${1#fixture-}"
}

has_label() {
    local wanted="$1" file="$2" line
    [ -f "${file}" ] || return 1
    while IFS= read -r line; do
        case "${wanted}" in
            *=*) [ "${line}" = "${wanted}" ] && return 0 ;;
            *) [ "${line%%=*}" = "${wanted}" ] && return 0 ;;
        esac
    done <"${file}"
    return 1
}

matching_entries() {
    local kind="$1" filters=() argument entry name filter matched
    shift
    while [ $# -gt 0 ]; do
        argument="$1"
        shift
        case "${argument}" in
            -f|--filter) filters+=("${1:-}"); shift ;;
            --filter=*) filters+=("${argument#--filter=}") ;;
            --format) shift ;;
        esac
    done
    for entry in "${state}/${kind}"/*; do
        [ -d "${entry}" ] || continue
        name="${entry##*/}"
        matched=1
        for filter in "${filters[@]+"${filters[@]}"}"; do
            case "${filter}" in
                label=*) has_label "${filter#label=}" "${entry}/labels" || matched=0 ;;
                name=*)
                    case "${name}" in
                        *"${filter#name=}"*) ;;
                        *) matched=0 ;;
                    esac
                    ;;
            esac
        done
        [ "${matched}" = 0 ] || printf '%s\n' "${name}"
    done
}

image_identity() {
    local reference="$1" repository digest='' name
    repository="${reference%%@*}"
    case "${reference}" in
        *@sha256:*) digest="${reference#*@}" ;;
    esac
    name="${repository##*/}"
    case "${name}" in
        *:*) repository="${repository%:*}" ;;
    esac
    if [ "$(cat "${state}/identity")" = pinned ] && [ -n "${digest}" ]; then
        printf '%s@%s\n' "${repository}" "${digest}"
    else
        printf '%s@%s\n' "${repository}" '@SUBSTITUTE@'
    fi
}

run_container() {
    local name='' network='' image='' labels='' publications='' argument
    while [ $# -gt 0 ]; do
        argument="$1"
        shift
        case "${argument}" in
            --name) name="${1:-}"; shift ;;
            --name=*) name="${argument#--name=}" ;;
            --network|--net) network="${1:-}"; shift ;;
            --network=*|--net=*) network="${argument#*=}" ;;
            -l|--label) labels+="${1:-}"$'\n'; shift ;;
            --label=*) labels+="${argument#--label=}"$'\n' ;;
            -p|--publish) publications+="${1:-} "; shift ;;
            --publish=*) publications+="${argument#--publish=} " ;;
            -e|--env|--env-file|-v|--volume|--mount|-w|--workdir|--entrypoint|-u|--user|-h|--hostname|--network-alias|--health-cmd|--health-interval|--health-timeout|--health-retries|--health-start-period|--stop-timeout|-m|--memory|--cpus|--pull|--platform|--tmpfs|--add-host|--restart|--log-driver|--log-opt)
                shift
                ;;
            -*) ;;
            *) image="${argument}"; break ;;
        esac
    done
    [ -n "${image}" ] || fail 'no image was named' 125
    printf '%s\n' "${image}" >>"${state}/images.log"
    case "${image}" in
        @MISSING@*) fail "Unable to find image '${image}' locally" 125 ;;
    esac
    [ -n "${name}" ] || name="anonymous-$$-${RANDOM}"
    [ ! -e "${state}/containers/${name}" ] || fail "container name ${name} is already in use" 125
    [ -z "${network}" ] || [ -d "${state}/networks/${network}" ] || fail "network ${network} not found" 125
    mkdir "${state}/containers/${name}"
    cp -R "${state}/image-files" "${state}/containers/${name}/files"
    printf '%s' "${labels}" >"${state}/containers/${name}/labels"
    sleep 600 </dev/null >/dev/null 2>&1 &
    printf '%s\n' "$!" >"${state}/containers/${name}/pid"
    printf '%s\t%s\t%s\t%s\t%s\n' "${name}" "$!" "${image}" "${network}" "${publications% }" \
        >>"${state}/started.log"
    printf 'fixture-%s\n' "${name}"
}

container_file() {
    local operand="$1" name
    case "${operand}" in
        *:*)
            name=$(require_container "${operand%%:*}") || return 1
            printf '%s/containers/%s/files/%s\n' "${state}" "${name}" "${operand#*:/}"
            ;;
        *) printf '%s\n' "${operand}" ;;
    esac
}

copy_file() {
    local source destination
    source=$(container_file "$1") || return 1
    destination=$(container_file "$2") || return 1
    [ -f "${source}" ] || fail "copy source not found: $1"
    mkdir -p "$(dirname "${destination}")"
    cp "${source}" "${destination}"
}

remove_containers() {
    local argument name pid status=0
    for argument in "$@"; do
        case "${argument}" in
            -*) continue ;;
        esac
        name=$(resolve "${argument}")
        if [ -d "${state}/containers/${name}" ]; then
            pid=$(cat "${state}/containers/${name}/pid")
            kill "${pid}" 2>/dev/null || true
            rm -rf "${state}/containers/${name}"
            printf '%s\n' "${name}"
        else
            printf 'Error response from daemon: No such container: %s\n' "${name}" >&2
            status=1
        fi
    done
    return "${status}"
}

prune_containers() {
    local name
    for name in $(matching_entries containers "$@"); do
        remove_containers "${name}" >/dev/null
    done
}

require_container() {
    local name
    name=$(resolve "$1")
    [ -d "${state}/containers/${name}" ] || fail "No such container: ${name}"
    printf '%s\n' "${name}"
}

inspect_container() {
    local name="$1" format="$2"
    case "${format}" in
        *Health*) printf 'healthy\n' ;;
        *Running*) printf 'true\n' ;;
        *Status*) printf 'running\n' ;;
        *) printf '[{"Name":"%s"}]\n' "${name}" ;;
    esac
}

inspect_command() {
    local kind="$1" format='' argument operand='' name
    shift
    while [ $# -gt 0 ]; do
        argument="$1"
        shift
        case "${argument}" in
            -f|--format) format="${1:-}"; shift ;;
            --format=*) format="${argument#--format=}" ;;
            --type) shift ;;
            -*) ;;
            *) operand="${argument}" ;;
        esac
    done
    name=$(resolve "${operand}")
    case "${kind}" in
        container)
            [ -d "${state}/containers/${name}" ] || fail "No such container: ${name}"
            inspect_container "${name}" "${format}"
            ;;
        network)
            [ -d "${state}/networks/${operand}" ] || fail "No such network: ${operand}"
            printf '[{"Name":"%s"}]\n' "${operand}"
            ;;
        image)
            case "${format}" in
                '{{.Size}}')
                    case "$(cat "${state}/disk-samples" 2>/dev/null)" in
                        1) printf '1024\n' ;;
                        *) printf '2048\n' ;;
                    esac
                    ;;
                *) image_identity "${operand}" ;;
            esac
            ;;
        any)
            if [ -d "${state}/containers/${name}" ]; then
                inspect_container "${name}" "${format}"
            elif [ -d "${state}/networks/${operand}" ]; then
                printf '[{"Name":"%s"}]\n' "${operand}"
            else
                image_identity "${operand}"
            fi
            ;;
    esac
}

port_command() {
    local name service='' line
    name=$(require_container "${1:-}") || exit 1
    while IFS= read -r line; do
        case "${line}" in
            @SERVICE_LABEL@=*) service="${line#@SERVICE_LABEL@=}" ;;
        esac
    done <"${state}/containers/${name}/labels"
    if [ -n "${service}" ] && [ -f "${state}/endpoints/${service}" ]; then
        cat "${state}/endpoints/${service}"
    else
        printf '%s\n' '@UNPUBLISHED@'
    fi
}

network_command() {
    local subcommand="${1:-}" labels='' argument name status=0
    [ $# -eq 0 ] || shift
    case "${subcommand}" in
        create)
            name=''
            while [ $# -gt 0 ]; do
                argument="$1"
                shift
                case "${argument}" in
                    -l|--label) labels+="${1:-}"$'\n'; shift ;;
                    --label=*) labels+="${argument#--label=}"$'\n' ;;
                    -d|--driver|--subnet|--gateway|--ip-range|-o|--opt) shift ;;
                    -*) ;;
                    *) name="${argument}" ;;
                esac
            done
            [ -n "${name}" ] || fail 'network name is required'
            [ ! -e "${state}/networks/${name}" ] || fail "network with name ${name} already exists"
            mkdir "${state}/networks/${name}"
            printf '%s' "${labels}" >"${state}/networks/${name}/labels"
            printf 'fixture-net-%s\n' "${name}"
            ;;
        rm|remove)
            for argument in "$@"; do
                case "${argument}" in
                    -*) continue ;;
                esac
                if [ -d "${state}/networks/${argument}" ]; then
                    rm -rf "${state}/networks/${argument}"
                    printf '%s\n' "${argument}"
                else
                    printf 'Error response from daemon: No such network: %s\n' "${argument}" >&2
                    status=1
                fi
            done
            return "${status}"
            ;;
        inspect) inspect_command network "$@" ;;
        ls|list) matching_entries networks "$@" ;;
        prune)
            for name in $(matching_entries networks "$@"); do
                rm -rf "${state}/networks/${name}"
                printf '%s\n' "${name}"
            done
            ;;
        connect|disconnect) ;;
        *) fail "unknown network command: ${subcommand}" ;;
    esac
}

while [ $# -gt 0 ]; do
    case "$1" in
        --context|-c|--host|-H|--config|--log-level|-l) shift 2 ;;
        --*=*|-D|--debug|--tls|--tlsverify) shift ;;
        *) break ;;
    esac
done

command="${1:-}"
[ $# -eq 0 ] || shift
case "${command}" in
    system)
        samples=$(cat "${state}/disk-samples" 2>/dev/null || printf '0')
        printf '%s\n' "$((samples + 1))" >"${state}/disk-samples"
        refused=$(cat "${state}/fail-disk-sample" 2>/dev/null || printf '0')
        [ "$((samples + 1))" != "${refused}" ] || fail 'disk measurement unavailable'
        printf '{"Type":"Images","Size":"%sKiB"}\n' "$((samples + 1))"
        ;;
    version|info) printf 'fixture engine 27.0.0\n' ;;
    pull) image_identity "$(last_operand "$@")" ;;
    run|create) run_container "$@" ;;
    cp) copy_file "$@" ;;
    start|stop|kill|wait|restart) require_container "$(last_operand "$@")" ;;
    rm) remove_containers "$@" ;;
    port) port_command "$@" ;;
    logs) require_container "$(last_operand "$@")" >/dev/null; printf 'fixture engine log\n' ;;
    ps) matching_entries containers "$@" ;;
    inspect) inspect_command any "$@" ;;
    exec) require_container "$(last_operand "$@")" >/dev/null ;;
    container)
        subcommand="${1:-}"
        [ $# -eq 0 ] || shift
        case "${subcommand}" in
            run|create) run_container "$@" ;;
            rm|remove) remove_containers "$@" ;;
            ls|list|ps) matching_entries containers "$@" ;;
            inspect) inspect_command container "$@" ;;
            port) port_command "$@" ;;
            logs) require_container "$(last_operand "$@")" >/dev/null; printf 'fixture engine log\n' ;;
            start|stop|kill|wait|restart) require_container "$(last_operand "$@")" ;;
            prune) prune_containers "$@" ;;
            *) fail "unknown container command: ${subcommand}" ;;
        esac
        ;;
    image)
        subcommand="${1:-}"
        [ $# -eq 0 ] || shift
        case "${subcommand}" in
            inspect) inspect_command image "$@" ;;
            pull) image_identity "$(last_operand "$@")" ;;
            ls|list) printf 'fixture-image-id\n' ;;
            *) fail "unknown image command: ${subcommand}" ;;
        esac
        ;;
    network) network_command "$@" ;;
    *) fail "unknown command: ${command}" ;;
esac
"#;
