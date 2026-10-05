use std::net::TcpListener;
use std::path::PathBuf;

use crate::support::FixtureError;
use crate::support::process::{
    CONFIG_REFUSAL_BOUND, failed_checks, is_fifo, make_fifo, run_command_within, serve_command,
};

/// The observable effects one refused configuration must never reach.
///
/// - The secret is a FIFO with no writer: loading it blocks in `open`.
/// - The cache is a path the ACME owner would create.
/// - The upstream is a bound, never-accepting listener: a health probe or
///   proxy request leaves a connection in its backlog.
/// - The listener is a Unix socket path the CLI would create on bind.
///
/// A DNS provider request needs the loaded secret, so the secret sentinel
/// stands in front of every provider effect.
struct Sentinels {
    root: tempfile::TempDir,
    config: PathBuf,
    socket: PathBuf,
    secret: PathBuf,
    cache: PathBuf,
    missing_root: PathBuf,
    upstream: TcpListener,
}

impl Sentinels {
    fn create() -> Result<Self, FixtureError> {
        let root = tempfile::tempdir()?;
        let secret = root.path().join("token.fifo");
        make_fifo(&secret)?;
        let upstream = TcpListener::bind("127.0.0.1:0")?;
        upstream.set_nonblocking(true)?;
        Ok(Self {
            config: root.path().join("camber.toml"),
            socket: root.path().join("camber.sock"),
            cache: root.path().join("acme-cache"),
            missing_root: root.path().join("missing-root"),
            secret,
            upstream,
            root,
        })
    }

    fn upstream_url(&self) -> Result<String, FixtureError> {
        Ok(format!("http://{}", self.upstream.local_addr()?))
    }

    /// A site whose startup health probe reaches the upstream sentinel.
    fn probed_site(&self, host: &str, extra: &str) -> Result<String, FixtureError> {
        Ok(format!(
            "[[site]]\nhost = \"{host}\"\nproxy = \"{}\"\nhealth_check = \"/health\"\nhealth_interval = 300\n{extra}\n",
            self.upstream_url()?
        ))
    }

    fn dns01_tls(&self) -> String {
        format!(
            "[tls]\nauto = true\nemail = \"admin@example.com\"\nstaging = true\n\
             cache_dir = \"{}\"\ndns_provider = \"cloudflare\"\ndns_api_token_file = \"{}\"\n",
            self.cache.display(),
            self.secret.display()
        )
    }

    fn tls_alpn_tls(&self) -> String {
        format!(
            "[tls]\nauto = true\nemail = \"admin@example.com\"\nstaging = true\ncache_dir = \"{}\"\n",
            self.cache.display()
        )
    }

    fn write_config(&self, body: &str) -> Result<(), FixtureError> {
        std::fs::write(
            &self.config,
            format!("listen = \"unix:{}\"\n{body}", self.socket.display()),
        )?;
        Ok(())
    }

    fn upstream_was_contacted(&self) -> Result<bool, FixtureError> {
        match self.upstream.accept() {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn finish(self) -> Result<(), FixtureError> {
        drop(self.upstream);
        self.root.close()?;
        Ok(())
    }
}

struct AdmissionRow {
    name: &'static str,
    body: fn(&Sentinels) -> Result<String, FixtureError>,
}

fn admission_rows() -> impl Iterator<Item = AdmissionRow> {
    site_admission_rows()
        .into_iter()
        .chain(tls_admission_rows())
}

fn site_admission_rows() -> [AdmissionRow; 9] {
    [
        AdmissionRow {
            name: "unknown site field",
            body: |s| s.probed_site("app.test", "upstream_timeout = 5"),
        },
        AdmissionRow {
            name: "unknown top-level field",
            body: |s| {
                Ok(format!(
                    "listen_backlog = 16\n{}",
                    s.probed_site("app.test", "")?
                ))
            },
        },
        AdmissionRow {
            name: "duplicate site host",
            body: |s| {
                Ok(format!(
                    "{}{}",
                    s.probed_site("app.test", "")?,
                    s.probed_site("APP.test", "")?
                ))
            },
        },
        AdmissionRow {
            name: "wildcard site host",
            body: |s| s.probed_site("*.app.test", ""),
        },
        AdmissionRow {
            name: "site host port above 65535",
            body: |s| s.probed_site("app.test:65536", ""),
        },
        AdmissionRow {
            name: "empty site array",
            body: |_| Ok("site = []\n".to_owned()),
        },
        AdmissionRow {
            name: "proxy with credentials",
            body: |s| {
                let upstream = s.upstream.local_addr()?;
                Ok(format!(
                    "[[site]]\nhost = \"app.test\"\nproxy = \"http://user:secret@{upstream}\"\n\
                     health_check = \"/health\"\nhealth_interval = 300\n"
                ))
            },
        },
        AdmissionRow {
            name: "health interval without health check",
            body: |s| {
                Ok(format!(
                    "{}[[site]]\nhost = \"b.test\"\nproxy = \"{}\"\nhealth_interval = 5\n",
                    s.probed_site("a.test", "")?,
                    s.upstream_url()?
                ))
            },
        },
        AdmissionRow {
            name: "overlay with nonexistent root",
            body: |s| {
                s.probed_site(
                    "app.test",
                    &format!("root = \"{}\"", s.missing_root.display()),
                )
            },
        },
    ]
}

fn tls_admission_rows() -> [AdmissionRow; 3] {
    [
        AdmissionRow {
            name: "nonexistent root under DNS-01",
            body: |s| {
                Ok(format!(
                    "{}[[site]]\nhost = \"example.com\"\nroot = \"{}\"\n",
                    s.dns01_tls(),
                    s.missing_root.display()
                ))
            },
        },
        AdmissionRow {
            name: "unknown site field under DNS-01",
            body: |s| {
                Ok(format!(
                    "{}{}",
                    s.dns01_tls(),
                    s.probed_site("example.com", "upstream_timeout = 5")?
                ))
            },
        },
        AdmissionRow {
            name: "IP site under automatic TLS-ALPN-01",
            body: |s| {
                Ok(format!(
                    "{}{}",
                    s.tls_alpn_tls(),
                    s.probed_site("127.0.0.1", "")?
                ))
            },
        },
    ]
}

fn run_admission_row(row: &AdmissionRow) -> Result<(), FixtureError> {
    let sentinels = Sentinels::create()?;
    let observed = observe_admission(row, &sentinels);
    FixtureError::with_cleanup(observed, sentinels.finish())
}

fn observe_admission(row: &AdmissionRow, sentinels: &Sentinels) -> Result<(), FixtureError> {
    sentinels.write_config(&(row.body)(sentinels)?)?;
    let output = run_command_within(serve_command(&sentinels.config), CONFIG_REFUSAL_BOUND)?;
    let (exited, success, announced, stderr) = match &output {
        Some(output) => (
            true,
            output.status.success(),
            output.announced_listener(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ),
        None => (false, false, false, String::new()),
    };

    let checks = [
        (
            exited,
            "the CLI did not exit within the bound: it loaded the secret or served",
        ),
        (exited && !success, "the CLI did not exit with a failure"),
        (
            !exited || !stderr.trim().is_empty(),
            "the refusal printed no diagnostic",
        ),
        (!announced, "the CLI reported a listener"),
        (!sentinels.socket.exists(), "the listener socket was bound"),
        (!sentinels.cache.exists(), "the ACME cache was created"),
        (is_fifo(&sentinels.secret)?, "the secret FIFO was replaced"),
        (
            !sentinels.upstream_was_contacted()?,
            "the upstream was probed",
        ),
    ];
    match failed_checks(&checks) {
        None => Ok(()),
        Some(failed) => Err(FixtureError::new(format!("{failed}; stderr: {stderr}"))),
    }
}

/// 8.T2: `camber serve` refuses each invalid configuration before any
/// secret read, cache write, provider request, upstream probe, or listener
/// bind. Rows run concurrently, each owning its child, sentinels, and
/// temporary root; every failure is collected before the declared assertion.
#[test]
fn invalid_configuration_has_no_startup_side_effects() -> Result<(), FixtureError> {
    let rows: Box<[_]> = admission_rows().collect();
    let failures: Box<[String]> = std::thread::scope(|scope| {
        let handles: Box<[_]> = rows
            .iter()
            .map(|row| (row.name, scope.spawn(move || run_admission_row(row))))
            .collect();
        handles
            .into_iter()
            .filter_map(|(name, handle)| match handle.join() {
                Ok(Ok(())) => None,
                Ok(Err(reason)) => Some(format!("{name}: {reason}")),
                Err(_) => Some(format!("{name}: the row panicked")),
            })
            .collect()
    });
    assert!(
        failures.is_empty(),
        "M9 invalid site configuration reached startup: {} of {} rows failed:\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n")
    );
    Ok(())
}
