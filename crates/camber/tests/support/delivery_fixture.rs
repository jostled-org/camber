//! Owned fixtures for the repository's delivery hooks.
//!
//! The hooks under test are real Bash processes. Everything they reach outside
//! the checkout — the Rust toolchain, Pedant, cargo-deny — is a controlled
//! executable in a directory the fixture owns, placed ahead of a system-only
//! `PATH`. An installed tool can never answer for a stub, and a stub never
//! reports the outcome the hook is meant to decide.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::process::ChildGuard;
use super::temp_support::TempRoot;

/// The bound on one hook process, from spawn to reap.
pub const HOOK_BOUND: Duration = Duration::from_secs(120);

/// The only directories a hook may search beyond the fixture's own tools.
const SYSTEM_PATH: &str = "/usr/bin:/bin";

/// Records a workflow phase and applies the fixture's injected failure.
///
/// Silent on purpose: evidence naming the failed phase and status must come
/// from the runner, never from the stub that failed.
const RECORD_PHASE: &str = r#"record_phase() {
    [ -n "${CAMBER_FIXTURE_LOG:-}" ] || return 0
    printf '%s\n' "$1" >>"${CAMBER_FIXTURE_LOG}"
    [ "$1" != "${CAMBER_FIXTURE_FAIL_PHASE:-}" ] || exit "${CAMBER_FIXTURE_FAIL_STATUS}"
}
"#;

pub fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn repository_file(relative: &str) -> Box<[u8]> {
    let path = repository_root().join(relative);
    fs::read(&path)
        .map(Vec::into_boxed_slice)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// Resolve `name` on the test's own `PATH`, failing when it is absent.
pub fn ambient_executable(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(name))
                .find(|candidate| is_executable(candidate))
        })
        .unwrap_or_else(|| panic!("required fixture executable is unavailable: {name}"))
}

/// Fail when a system directory could answer for `name` in place of a stub.
pub fn assert_absent_from_system_path(name: &str) {
    for directory in SYSTEM_PATH.split(':') {
        let candidate = Path::new(directory).join(name);
        assert!(
            !is_executable(&candidate),
            "{} would answer for the fixture's absent {name}",
            candidate.display()
        );
    }
}

/// Whether `path` is an executable file.
///
/// Only a path that is not there reads as not executable. Any other failure to
/// inspect it fails the fixture, so an unreadable system directory can never
/// pass as one that holds no competing tool.
fn is_executable(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        Err(error) if is_absent(&error) => false,
        Err(error) => panic!("cannot inspect {}: {error}", path.display()),
    }
}

/// A lookup that failed because nothing is there: no entry, or a `PATH` entry
/// that is a file rather than a directory.
fn is_absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn quoted(path: &Path) -> String {
    let text = path.to_str().expect("fixture paths are UTF-8");
    assert!(
        !text.contains('\''),
        "fixture path cannot be single-quoted: {text}"
    );
    format!("'{text}'")
}

pub fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture directory was not created");
    }
    fs::write(path, bytes).expect("fixture file was not written");
}

pub fn write_executable(path: &Path, bytes: impl AsRef<[u8]>) {
    write_file(path, bytes.as_ref());
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .expect("fixture executable mode was not set");
}

/// A finished hook process: its exit code and everything it wrote.
pub struct HookRun {
    pub status: i32,
    pub output: Box<str>,
}

/// Run `command` to exit inside [`HOOK_BOUND`], reaping it and its readers.
pub fn run_bounded(mut command: Command) -> HookRun {
    command.stdin(Stdio::null());
    let mut child = ChildGuard::spawn(command, HOOK_BOUND).expect("hook process did not start");
    let status = child
        .wait_bounded(HOOK_BOUND)
        .expect("hook process did not finish inside its bound");
    let mut output = String::from_utf8_lossy(child.stdout()).into_owned();
    output.push_str(&String::from_utf8_lossy(child.stderr()));
    HookRun {
        status: status
            .code()
            .unwrap_or_else(|| panic!("hook process ended by a signal:\n{output}")),
        output: output.into_boxed_str(),
    }
}

/// A command with no ambient environment: only what the fixture names.
pub fn isolated_command(program: &Path, cwd: &Path, home: &Path, path: &str) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(cwd)
        .env_clear()
        .env("PATH", path)
        .env("HOME", home)
        .env("TMPDIR", home)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Camber Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@camber.invalid")
        .env("GIT_COMMITTER_NAME", "Camber Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@camber.invalid");
    command
}

/// The toolchain and delivery tools whose identity the workflow pins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Rustc,
    Cargo,
    Clippy,
    Rustfmt,
    Pedant,
    CargoDeny,
}

impl Tool {
    pub const ALL: [Self; 6] = [
        Self::Rustc,
        Self::Cargo,
        Self::Clippy,
        Self::Rustfmt,
        Self::Pedant,
        Self::CargoDeny,
    ];

    /// Tools a rustup proxy answers for, which rustup can install on demand.
    pub const RUSTUP_MANAGED: [Self; 4] = [Self::Rustc, Self::Cargo, Self::Clippy, Self::Rustfmt];

    /// Spellings a refusal may use to name this tool.
    pub fn names(self) -> &'static [&'static str] {
        match self {
            Self::Rustc => &["rustc"],
            Self::Cargo => &["cargo"],
            Self::Clippy => &["clippy"],
            Self::Rustfmt => &["rustfmt"],
            Self::Pedant => &["pedant"],
            Self::CargoDeny => &["cargo-deny", "cargo deny"],
        }
    }

    /// Executables that report this tool's identity. The first is the one the
    /// workflow resolves.
    pub fn executables(self) -> &'static [&'static str] {
        match self {
            Self::Rustc => &["rustc"],
            Self::Cargo => &["cargo"],
            Self::Clippy => &["cargo-clippy", "clippy-driver"],
            Self::Rustfmt => &["cargo-fmt", "rustfmt"],
            Self::Pedant => &["pedant"],
            Self::CargoDeny => &["cargo-deny"],
        }
    }

    /// The version line and release of the identity the plan pins.
    pub fn pinned(self) -> (&'static str, &'static str) {
        match self {
            Self::Rustc => ("rustc 1.98.0 (88d9e12ae 2026-08-18)", "1.98.0"),
            Self::Cargo => ("cargo 1.98.0 (797e8a9bc 2026-08-05)", "1.98.0"),
            Self::Clippy => ("clippy 0.1.98 (88d9e12ae1 2026-08-18)", "0.1.98"),
            Self::Rustfmt => ("rustfmt 1.9.0-stable (88d9e12ae1 2026-08-18)", "1.9.0"),
            Self::Pedant => ("pedant 0.30.1", "0.30.1"),
            Self::CargoDeny => ("cargo-deny 0.19.0", "0.19.0"),
        }
    }

    /// A well-formed identity one release away from the pinned one.
    fn mismatched(self) -> (&'static str, &'static str) {
        match self {
            Self::Rustc => ("rustc 1.97.0 (51ff8ff5d 2026-07-07)", "1.97.0"),
            Self::Cargo => ("cargo 1.97.0 (6f2c0a1b3 2026-06-24)", "1.97.0"),
            Self::Clippy => ("clippy 0.1.97 (51ff8ff5d0 2026-07-07)", "0.1.97"),
            Self::Rustfmt => ("rustfmt 1.8.0-stable (51ff8ff5d0 2026-07-07)", "1.8.0"),
            Self::Pedant => ("pedant 0.30.0", "0.30.0"),
            Self::CargoDeny => ("cargo-deny 0.18.9", "0.18.9"),
        }
    }
}

/// What one tool's executables report when asked for their version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    Pinned,
    Mismatched,
    Unreadable,
    Missing,
    /// A rustup proxy whose toolchain or component is absent. It installs the
    /// pinned identity and answers with it unless auto-install is disabled.
    Uninstalled,
}

/// What `pedant supply-chain verify` prints and returns.
#[derive(Clone, Copy, Debug)]
pub struct PedantVerify<'a> {
    pub output: &'a str,
    pub status: i32,
}

impl PedantVerify<'static> {
    pub const CLEAN: Self = Self {
        output: "",
        status: 0,
    };
}

/// A directory of controlled executables, searched before the system path.
pub struct ToolBin {
    root: TempRoot,
}

impl ToolBin {
    pub fn pinned() -> Self {
        Self::new(|_| Identity::Pinned, PedantVerify::CLEAN)
    }

    pub fn new(identity: impl Fn(Tool) -> Identity, verify: PedantVerify<'_>) -> Self {
        let root = TempRoot::new().expect("tool fixture root was not created");
        let tools = Self { root };
        fs::create_dir_all(tools.bin()).expect("tool directory was not created");
        fs::create_dir_all(tools.home()).expect("tool home was not created");
        std::os::unix::fs::symlink(ambient_executable("bash"), tools.bin().join("bash"))
            .expect("bash was not linked");
        std::os::unix::fs::symlink(ambient_executable("rg"), tools.bin().join("rg"))
            .expect("rg was not linked");
        write_executable(&tools.bin().join("git"), delegating_git());
        write_executable(
            &tools.bin().join("protoc"),
            "#!/usr/bin/env bash\nprintf 'libprotoc 29.3\\n'\n",
        );
        write_file(
            &tools.verify_output(),
            verify_output(verify.output).as_bytes(),
        );
        for tool in Tool::ALL {
            tools.install(tool, identity(tool), verify.status);
        }
        tools
    }

    fn install(&self, tool: Tool, identity: Identity, verify_status: i32) {
        let proxy = match identity {
            Identity::Uninstalled => self.uninstalled_proxy(tool),
            _ => String::new(),
        };
        let version = match identity {
            Identity::Missing => return,
            Identity::Pinned | Identity::Uninstalled => version_reply(tool.pinned()),
            Identity::Mismatched => version_reply(tool.mismatched()),
            Identity::Unreadable => {
                "printf 'fixture: version is unavailable\\n' >&2\n        exit 1".to_owned()
            }
        };
        let body = match tool {
            Tool::Cargo => return self.install_cargo(&proxy, &version),
            Tool::Pedant => format!(
                "if [ \"${{1:-}}\" = supply-chain ] && [ \"${{2:-}}\" = verify ]; then\n    \
                 printf 'verify\\n' >>{log}\n    cat {output}\n    exit {verify_status}\n\
                 fi\n",
                log = quoted(&self.verify_log()),
                output = quoted(&self.verify_output()),
            ),
            _ => String::new(),
        };
        let script = format!(
            "#!/usr/bin/env bash\n\
             {proxy}\
             for argument in \"$@\"; do\n    \
             case \"${{argument}}\" in\n        \
             --version|-V|-vV)\n        {version}\n        ;;\n    \
             esac\n\
             done\n\
             {body}exit 0\n"
        );
        for executable in tool.executables() {
            write_executable(&self.bin().join(executable), &script);
        }
    }

    /// Rustup's answer for an absent toolchain or component: refuse when
    /// auto-install is disabled, otherwise record an install and carry on.
    fn uninstalled_proxy(&self, tool: Tool) -> String {
        let refusal = match tool {
            Tool::Rustc | Tool::Cargo => "error: toolchain '1.98.0-fixture' is not installed",
            _ => "error: '$(basename \"$0\")' is not installed for the toolchain '1.98.0-fixture'",
        };
        format!(
            "if [ \"${{RUSTUP_AUTO_INSTALL:-1}}\" = 0 ]; then\n    \
             printf '%s\\n' \"{refusal}\" >&2\n    \
             exit 1\n\
             fi\n\
             printf 'install\\n' >>{log}\n",
            log = quoted(&self.install_log()),
        )
    }

    fn install_cargo(&self, proxy: &str, version: &str) {
        let script = format!(
            r#"#!/usr/bin/env bash
{RECORD_PHASE}
{proxy}
subcommand=''
takes_value=0
for argument in "$@"; do
    if [ "${{takes_value}}" = 1 ]; then
        takes_value=0
        continue
    fi
    case "${{argument}}" in
        --config|-Z|-C) takes_value=1 ;;
        +*|-*) ;;
        *) subcommand="${{argument}}"; break ;;
    esac
done
for argument in "$@"; do
    case "${{argument}}" in
        --version|-V|-vV)
            case "${{subcommand}}" in
                clippy|fmt|deny)
                    command -v "cargo-${{subcommand}}" >/dev/null 2>&1 || {{
                        printf 'error: no such command: `%s`\n' "${{subcommand}}" >&2
                        exit 101
                    }}
                    exec "cargo-${{subcommand}}" "$@"
                    ;;
            esac
            {version}
            ;;
    esac
done
case "${{subcommand}}" in
    fmt|clippy|doc|test|deny) record_phase "${{subcommand}}" ;;
esac
exit 0
"#
        );
        write_executable(&self.bin().join("cargo"), &script);
    }

    pub fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// `PATH` for a hook: these tools first, then the system directories.
    pub fn search_path(&self) -> String {
        format!("{}:{SYSTEM_PATH}", self.bin().display())
    }

    fn verify_log(&self) -> PathBuf {
        self.root.path().join("pedant-verify.log")
    }

    fn verify_output(&self) -> PathBuf {
        self.root.path().join("pedant-verify.out")
    }

    fn install_log(&self) -> PathBuf {
        self.root.path().join("rustup-install.log")
    }

    /// How many times a rustup proxy installed an absent toolchain or component.
    pub fn install_calls(&self) -> usize {
        logged_calls(&self.install_log())
    }

    /// How many times a hook ran `pedant supply-chain verify`.
    pub fn verify_calls(&self) -> usize {
        logged_calls(&self.verify_log())
    }

    pub fn close(self) {
        self.root
            .close()
            .expect("tool fixture root was not removed");
    }
}

/// The lines a stub appended to `log`, or zero when no stub ever wrote it.
fn logged_calls(log: &Path) -> usize {
    logged_text(log).lines().count()
}

/// Everything stubs appended to `log`, and nothing when no stub ever wrote it.
///
/// Only an absent log reads as empty. A log that exists and cannot be read
/// fails the fixture rather than reading as no record.
pub fn logged_text(log: &Path) -> Box<str> {
    match fs::read_to_string(log) {
        Ok(text) => text.into_boxed_str(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Box::default(),
        Err(error) => panic!("cannot read {}: {error}", log.display()),
    }
}

fn version_reply((line, release): (&str, &str)) -> String {
    format!(
        "case \"${{argument}}\" in\n            \
         -vV) printf '%s\\n' '{line}' 'release: {release}' ;;\n            \
         *) printf '%s\\n' '{line}' ;;\n        \
         esac\n        \
         exit 0"
    )
}

fn verify_output(output: &str) -> String {
    match output.is_empty() {
        true => String::new(),
        false => format!("{output}\n"),
    }
}

/// Real Git, except a `worktree remove` the fixture tells to fail.
fn delegating_git() -> String {
    format!(
        "#!/usr/bin/env bash\n\
         case \" $* \" in\n    \
         *' worktree remove '*) [ \"${{CAMBER_FIXTURE_FAIL_CLEANUP:-0}}\" != 1 ] || exit 1 ;;\n\
         esac\n\
         exec {git} \"$@\"\n",
        git = quoted(&ambient_executable("git")),
    )
}

/// A hook stub that records `phase` as it runs.
pub fn phase_hook(phase: &str) -> String {
    format!("#!/usr/bin/env bash\n{RECORD_PHASE}\nrecord_phase {phase}\n")
}

/// An owned Git repository, with a home and scratch directory beside it.
pub struct FixtureRepo {
    root: TempRoot,
    /// Real Git, resolved once rather than on every fixture command.
    git: PathBuf,
    /// `PATH` for fixture Git: its own directory, then the system ones.
    git_search_path: Box<str>,
}

impl FixtureRepo {
    pub fn new() -> Self {
        let root = TempRoot::new().expect("repository fixture root was not created");
        let git = ambient_executable("git");
        let git_search_path = format!(
            "{}:{SYSTEM_PATH}",
            git.parent().expect("git has a directory").display()
        )
        .into_boxed_str();
        let repo = Self {
            root,
            git,
            git_search_path,
        };
        for directory in [repo.path(), repo.home(), repo.scratch()] {
            fs::create_dir_all(directory).expect("repository fixture directory was not created");
        }
        repo.git(&["init", "--quiet", "--initial-branch=main"]);
        repo
    }

    pub fn path(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// An owned directory for a hook's `TMPDIR`, empty until a hook uses it.
    pub fn scratch(&self) -> PathBuf {
        self.root.path().join("scratch")
    }

    /// A file outside the repository, so writing it leaves the tree clean.
    pub fn outside(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    pub fn write(&self, relative: &str, bytes: &[u8]) {
        write_file(&self.path().join(relative), bytes);
    }

    pub fn write_executable(&self, relative: &str, bytes: impl AsRef<[u8]>) {
        write_executable(&self.path().join(relative), bytes);
    }

    pub fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.path().join(relative)).expect("fixture file was not read")
    }

    pub fn remove(&self, relative: &str) {
        fs::remove_file(self.path().join(relative)).expect("fixture file was not removed");
    }

    /// Commit the whole tree and return the new commit's identity.
    pub fn commit(&self, message: &str) -> Box<str> {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"]).output.trim().into()
    }

    /// Run real Git in the repository, requiring success.
    pub fn git(&self, args: &[&str]) -> HookRun {
        let mut command =
            isolated_command(&self.git, &self.path(), &self.home(), &self.git_search_path);
        command.args(args);
        let run = run_bounded(command);
        assert_eq!(
            run.status, 0,
            "fixture git {args:?} failed:\n{}",
            run.output
        );
        run
    }

    /// A command run from the repository that searches only the system
    /// directories.
    pub fn system_command(&self, program: &Path) -> Command {
        isolated_command(program, &self.path(), &self.home(), SYSTEM_PATH)
    }

    /// A hook command run from the repository with `tools` on its path.
    pub fn command(&self, program: &Path, tools: &ToolBin) -> Command {
        let mut command =
            isolated_command(program, &self.path(), &self.home(), &tools.search_path());
        command.env("TMPDIR", self.scratch());
        command
    }

    pub fn close(self) {
        self.root
            .close()
            .expect("repository fixture root was not removed");
    }
}

impl Default for FixtureRepo {
    fn default() -> Self {
        Self::new()
    }
}
