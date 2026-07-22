use super::BoundListener;
use std::sync::atomic::Ordering;

#[test]
fn bound_listener_transfers_without_rebind() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(async {
            let reservation = BoundListener::bind_tcp("127.0.0.1:0")?;
            let reserved_addr = reservation.local_addr();
            let mut router = camber::http::Router::new();
            router.get("/", |_request: &camber::http::Request| async {
                camber::http::Response::text(200, "transferred")
            });
            let server =
                super::ReadyServer::start(reservation, router, std::time::Duration::from_secs(1))?;
            let cleanup = server.cleanup_probe();
            let second_bind = std::net::TcpListener::bind(server.local_addr());
            assert_eq!(
                second_bind.unwrap_err().kind(),
                std::io::ErrorKind::AddrInUse
            );
            let response = super::request(
                reserved_addr,
                "GET",
                "/",
                &[],
                &[],
                std::time::Duration::from_secs(1),
            )?;
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"transferred");
            server.shutdown_bounded(std::time::Duration::from_secs(1))?;
            assert!(cleanup.joined());
            assert_eq!(cleanup.cleanup_error(), None);
            Ok::<_, super::FixtureError>(())
        })
        .unwrap();
}

#[test]
fn server_readiness_is_signaled_without_sleep() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(async {
            let mut router = camber::http::Router::new();
            router.get("/", |_request: &camber::http::Request| async {
                camber::http::Response::text(200, "ready")
            });
            let server = super::spawn_server_ready(router, std::time::Duration::from_secs(1))?;
            assert!(server.local_addr().ip().is_loopback());
            assert_eq!(server.readiness_response().status, 400);
            server.shutdown_bounded(std::time::Duration::from_secs(1))?;
            Ok::<_, super::FixtureError>(())
        })
        .unwrap();
}

#[test]
fn bounded_read_reports_timeout() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_server, _) = listener.accept().unwrap();

    let result = super::bounded_read(&mut client, std::time::Duration::from_millis(25), 1024);
    assert!(matches!(
        result,
        Err(super::BoundedReadError::Timeout { .. })
    ));
}

#[test]
fn child_guard_reaps_after_assertion_panic() {
    const MODE: &str = "panic-cleanup";
    if super::is_private_child(MODE) {
        println!("FIXTURE_READY");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        let (_sender, receiver) = std::sync::mpsc::channel::<()>();
        let result = receiver.recv_timeout(std::time::Duration::from_secs(30));
        assert!(result.is_ok(), "fixture parent did not terminate child");
        return;
    }

    let mut child = super::ChildGuard::spawn_exact_current(
        "fixture_contracts::child_guard_reaps_after_assertion_panic",
        MODE,
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    let child_id = child.id();
    let probe = child.take_reap_probe().unwrap();
    child
        .wait_for_line("FIXTURE_READY", std::time::Duration::from_secs(2))
        .unwrap();

    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _owned_child = child;
        assert_eq!(1, 2, "intentional fixture assertion failure");
    }));
    assert!(panic_result.is_err());
    assert_eq!(
        probe
            .wait(std::time::Duration::from_secs(2))
            .unwrap()
            .child_id(),
        child_id
    );
}

#[test]
fn child_guard_kills_and_waits_after_readiness_timeout() {
    const MODE: &str = "readiness-timeout";
    if super::is_private_child(MODE) {
        println!("CHILD_STARTED");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        let (_sender, receiver) = std::sync::mpsc::channel::<()>();
        let result = receiver.recv_timeout(std::time::Duration::from_secs(30));
        assert!(result.is_ok(), "fixture parent did not terminate child");
        return;
    }

    let mut child = super::ChildGuard::spawn_exact_current(
        "fixture_contracts::child_guard_kills_and_waits_after_readiness_timeout",
        MODE,
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    let child_id = child.id();
    let probe = child.take_reap_probe().unwrap();
    child
        .wait_for_line("CHILD_STARTED", std::time::Duration::from_secs(2))
        .unwrap();

    let result = child.wait_for_readiness("SERVER_READY", std::time::Duration::from_millis(25));
    assert!(matches!(
        result,
        Err(super::ProcessError::ReadinessTimeout { .. })
    ));
    let reaped = probe.wait(std::time::Duration::from_secs(2)).unwrap();
    assert_eq!(reaped.child_id(), child_id);
    assert!(!reaped.status().success());
}

#[test]
fn isolated_global_contract_runs_once_in_child() {
    const METRICS_MODE: &str = "isolated-metrics";
    const TEXT_MODE: &str = "isolated-logging-text";
    const JSON_MODE: &str = "isolated-logging-json";
    if super::is_private_child(METRICS_MODE) {
        super::test_runtime()
            .with_metrics()
            .run(|| {
                metrics::counter!("phase3_fixture_metrics_total").increment(1);
                println!("METRICS_INITIALIZED_ONCE");
            })
            .unwrap();
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        return;
    }
    if super::is_private_child(TEXT_MODE) {
        camber::logging::init_logging(
            camber::logging::LogFormat::Text,
            camber::logging::LogLevel::Info,
        );
        camber::tracing::info!(message = "PHASE3_TEXT_LOG_EVENT");
        println!("TEXT_LOGGING_INITIALIZED_ONCE");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        return;
    }
    if super::is_private_child(JSON_MODE) {
        camber::logging::init_logging(
            camber::logging::LogFormat::Json,
            camber::logging::LogLevel::Info,
        );
        camber::tracing::info!(message = "PHASE3_JSON_LOG_EVENT");
        println!("JSON_LOGGING_INITIALIZED_ONCE");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        return;
    }

    let metrics = super::run_isolated_exact(
        "fixture_contracts::isolated_global_contract_runs_once_in_child",
        METRICS_MODE,
        "METRICS_INITIALIZED_ONCE",
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    assert!(metrics.success());
    assert_output_once(&metrics, "METRICS_INITIALIZED_ONCE");
    let text = super::run_isolated_exact(
        "fixture_contracts::isolated_global_contract_runs_once_in_child",
        TEXT_MODE,
        "TEXT_LOGGING_INITIALIZED_ONCE",
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    assert!(text.success());
    assert_output_once(&text, "TEXT_LOGGING_INITIALIZED_ONCE");
    assert_output_once(&text, "PHASE3_TEXT_LOG_EVENT");
    let json = super::run_isolated_exact(
        "fixture_contracts::isolated_global_contract_runs_once_in_child",
        JSON_MODE,
        "JSON_LOGGING_INITIALIZED_ONCE",
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    assert!(json.success());
    assert_output_once(&json, "JSON_LOGGING_INITIALIZED_ONCE");
    assert_output_once(&json, "\"message\":\"PHASE3_JSON_LOG_EVENT\"");
}

#[cfg(unix)]
#[test]
fn signal_contract_cannot_reach_parent_process() {
    const SIGINT_MODE: &str = "isolated-sigint";
    const SIGTERM_MODE: &str = "isolated-sigterm";
    let child_mode = match (
        super::is_private_child(SIGINT_MODE),
        super::is_private_child(SIGTERM_MODE),
    ) {
        (true, false) => Some((SIGINT_MODE, signal_hook::consts::SIGINT)),
        (false, true) => Some((SIGTERM_MODE, signal_hook::consts::SIGTERM)),
        _ => None,
    };
    if let Some((mode, signal)) = child_mode {
        let parent_id = super::private_child_parent_id().unwrap();
        assert_ne!(parent_id, std::process::id());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let notify = std::sync::Arc::new(tokio::sync::Notify::new());
            let watcher =
                camber::signals::spawn_signal_watcher(std::sync::Arc::clone(&shutdown), notify);
            signal_hook::low_level::raise(signal).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), watcher)
                .await
                .unwrap()
                .unwrap();
            assert!(shutdown.load(Ordering::Acquire));
        });
        println!("CHILD_SIGNAL_CONTRACT_COMPLETE={mode}");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        return;
    }

    let parent_id = std::process::id();
    let sigint = super::run_signal_contract_exact(
        "fixture_contracts::signal_contract_cannot_reach_parent_process",
        SIGINT_MODE,
        "CHILD_SIGNAL_CONTRACT_COMPLETE=isolated-sigint",
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    assert!(sigint.success());
    let sigterm = super::run_signal_contract_exact(
        "fixture_contracts::signal_contract_cannot_reach_parent_process",
        SIGTERM_MODE,
        "CHILD_SIGNAL_CONTRACT_COMPLETE=isolated-sigterm",
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    assert!(sigterm.success());
    assert_eq!(std::process::id(), parent_id);
}

#[test]
fn complete_http_response_parses_chunked_body_without_eof() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let mut server = Some(std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        std::io::Write::write_all(
            &mut stream,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Test: complete\r\n\r\n",
        )
        .unwrap();
        release_receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
    }));
    let mut client = std::net::TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    let response = super::read_http_response(&mut client).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_ref(), b"Wikipedia");
    assert_eq!(response.header("connection"), Some("keep-alive"));
    release_sender.send(()).unwrap();
    super::join_thread_bounded(&mut server, std::time::Duration::from_secs(1)).unwrap();
}

#[test]
fn server_guard_joins_after_assertion_panic() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let server = super::spawn_server_ready(
            camber::http::Router::new(),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        let cleanup = server.cleanup_probe();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _server = server;
            assert_eq!(1, 2, "intentional server fixture assertion failure");
        }));
        assert!(unwind.is_err());
        assert!(cleanup.joined());
        assert_eq!(cleanup.cleanup_error(), None);
    });
}

#[test]
fn child_guard_reaps_after_normal_success() {
    const MODE: &str = "normal-cleanup";
    if super::is_private_child(MODE) {
        println!("NORMAL_COMPLETE");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        std::io::Write::write_all(&mut std::io::stderr(), b"NORMAL_STDERR\n").unwrap();
        std::io::Write::flush(&mut std::io::stderr()).unwrap();
        return;
    }
    let mut child = super::ChildGuard::spawn_exact_current(
        "fixture_contracts::child_guard_reaps_after_normal_success",
        MODE,
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    let probe = child.take_reap_probe().unwrap();
    child
        .wait_for_line("NORMAL_COMPLETE", std::time::Duration::from_secs(2))
        .unwrap();
    assert!(
        child
            .wait_bounded(std::time::Duration::from_secs(2))
            .unwrap()
            .success()
    );
    assert!(
        child
            .stdout()
            .windows(15)
            .any(|bytes| bytes == b"NORMAL_COMPLETE")
    );
    assert!(
        child
            .stderr()
            .windows(13)
            .any(|bytes| bytes == b"NORMAL_STDERR")
    );
    assert!(
        probe
            .wait(std::time::Duration::from_secs(2))
            .unwrap()
            .status()
            .success()
    );
}

#[test]
fn child_guard_reaps_after_child_timeout() {
    const MODE: &str = "child-timeout";
    if super::is_private_child(MODE) {
        println!("CHILD_HOLDING");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        let (_sender, receiver) = std::sync::mpsc::channel::<()>();
        let result = receiver.recv_timeout(std::time::Duration::from_secs(30));
        assert!(result.is_ok(), "fixture parent did not terminate child");
        return;
    }
    let mut child = super::ChildGuard::spawn_exact_current(
        "fixture_contracts::child_guard_reaps_after_child_timeout",
        MODE,
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    let probe = child.take_reap_probe().unwrap();
    child
        .wait_for_line("CHILD_HOLDING", std::time::Duration::from_secs(2))
        .unwrap();
    assert!(matches!(
        child.wait_bounded(std::time::Duration::ZERO),
        Err(super::ProcessError::ExitTimeout { .. })
    ));
    assert!(
        !probe
            .wait(std::time::Duration::from_secs(2))
            .unwrap()
            .status()
            .success()
    );
}

#[test]
fn temp_root_cleans_up_during_assertion_unwind() {
    let root = super::TempRoot::new().unwrap();
    let path = root.path().to_owned();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _root = root;
        assert_eq!(1, 2, "intentional temporary-root assertion failure");
    }));
    assert!(result.is_err());
    assert!(!path.exists());
}

#[test]
fn external_resource_names_are_unique_and_safe() {
    let first = super::unique_external_name("Camber Fixture");
    let second = super::unique_external_name("Camber Fixture");
    assert_ne!(first, second);
    assert!(first.starts_with("camber-fixture-"));
    assert!(first.chars().all(|character| character.is_ascii_lowercase()
        || character.is_ascii_digit()
        || character == '-'));
}

fn assert_output_once(run: &super::IsolatedRun, expected: &str) {
    let count = String::from_utf8_lossy(run.stdout())
        .matches(expected)
        .count()
        + String::from_utf8_lossy(run.stderr())
            .matches(expected)
            .count();
    assert_eq!(count, 1, "expected one child marker for {expected:?}");
}
