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
fn paused_checkpoint_wait_reports_a_missing_observation() {
    let controller = camber::runtime_test_support::runtime_schedule();
    let never = camber::runtime_test_support::RuntimeCheckpoint::ScopeWaitObserved(0);

    // Nothing armed it and no runtime will reach it, so the wait must return
    // at its bound: an observer that parked here would hang the drain-window
    // fixtures instead of failing them.
    assert!(!super::wait_paused_bounded(
        &controller,
        never,
        std::time::Duration::from_millis(25)
    ));
}

#[test]
fn paused_window_probe_reports_a_window_that_never_opened() {
    let controller = camber::runtime_test_support::runtime_schedule();
    let never = camber::runtime_test_support::RuntimeCheckpoint::ScopeWaitObserved(0);
    let probed = std::cell::Cell::new(false);

    // Nothing armed it, so the window never opens. The helper must report the
    // miss at its bound and leave the probe unrun: a fixture that read a
    // default observation as a real one would assert on a reading production
    // never produced.
    let observed = super::probe_paused_window(
        &controller,
        never,
        std::time::Duration::from_millis(25),
        || probed.set(true),
    );

    assert_eq!(observed, None);
    assert!(!probed.get(), "the probe ran on a window that never opened");
}

#[test]
fn paused_window_probe_leaves_an_armed_checkpoint_holding_nothing() {
    let controller = camber::runtime_test_support::runtime_schedule();
    let armed = camber::runtime_test_support::RuntimeCheckpoint::ScopeWaitObserved(0);
    let next = camber::runtime_test_support::RuntimeCheckpoint::ScopeWaitObserved(1);

    controller.pause_once(armed).unwrap();
    let observed = super::probe_paused_window(
        &controller,
        armed,
        std::time::Duration::from_millis(25),
        || (),
    );
    assert_eq!(observed, None);

    // The checkpoint was armed and nothing ever paused at it, which is the case
    // `release` cannot clear — it reports an error precisely when nothing is
    // paused. `arm` refuses while anything is still armed, so this succeeding
    // is what says the guard left nothing behind. Left armed, the next
    // production run to reach it would park with no observer to let it go.
    assert!(
        controller.pause_once(next).is_ok(),
        "the expired probe left its checkpoint armed"
    );
    controller.disarm();
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
    if hold_until_parent_kills(MODE, "FIXTURE_READY") {
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
    if hold_until_parent_kills(MODE, "CHILD_STARTED") {
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
    let sigint = super::run_isolated_exact(
        "fixture_contracts::signal_contract_cannot_reach_parent_process",
        SIGINT_MODE,
        "CHILD_SIGNAL_CONTRACT_COMPLETE=isolated-sigint",
        std::time::Duration::from_secs(2),
    )
    .unwrap();
    assert!(sigint.success());
    let sigterm = super::run_isolated_exact(
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
    // One deadline over the whole response, not a bound per read: the peer holds
    // the connection open after the terminating chunk, so a reader given a fresh
    // budget per syscall would have nothing to expire against.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let response = super::read_http_response(&mut client, Some(deadline)).unwrap();
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
    // The markers the child writes and the parent searches for, spelled once:
    // a hand-counted window length is a second spelling that drifts the moment
    // a marker is renamed. Spelled as text, because the line wait takes text
    // and only the byte searches need the free conversion back.
    const COMPLETE: &str = "NORMAL_COMPLETE";
    const STDERR: &str = "NORMAL_STDERR";
    if super::is_private_child(MODE) {
        std::io::Write::write_all(&mut std::io::stdout(), COMPLETE.as_bytes()).unwrap();
        std::io::Write::write_all(&mut std::io::stdout(), b"\n").unwrap();
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        std::io::Write::write_all(&mut std::io::stderr(), STDERR.as_bytes()).unwrap();
        std::io::Write::write_all(&mut std::io::stderr(), b"\n").unwrap();
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
        .wait_for_line(COMPLETE, std::time::Duration::from_secs(2))
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
            .windows(COMPLETE.len())
            .any(|bytes| bytes == COMPLETE.as_bytes())
    );
    assert!(
        child
            .stderr()
            .windows(STDERR.len())
            .any(|bytes| bytes == STDERR.as_bytes())
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
    if hold_until_parent_kills(MODE, "CHILD_HOLDING") {
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

#[test]
fn capture_bus_records_only_the_needle_that_asked() {
    const WATCHED: &str = "camber-capture-contract-watched";
    const UNWATCHED: &str = "camber-capture-contract-unwatched";
    const REASON: &str = "capture-bus-contract";

    let watched = super::capture_events(WATCHED);
    let unwatched = super::capture_events(UNWATCHED);
    camber::tracing::info!(subject = WATCHED, reason = REASON);

    // Both halves matter: a bus that recorded nothing and a bus that recorded
    // everything both leave a needle-scoped assertion looking healthy on its
    // own, and only the pair separates them.
    assert!(
        watched.recorded(&[WATCHED, REASON]),
        "the capture bus did not record the event naming its needle"
    );
    assert_eq!(
        unwatched.len(),
        0,
        "the capture bus recorded an event that never named this needle"
    );
}

#[test]
fn capture_bus_unsubscribes_when_its_handle_drops() {
    const NEEDLE: &str = "camber-capture-contract-dropped";

    assert_eq!(super::captures_for(NEEDLE), 0);
    let capture = super::capture_events(NEEDLE);
    assert_eq!(super::captures_for(NEEDLE), 1);
    drop(capture);

    // A subscription outliving its handle would keep pushing into a buffer no
    // test reads, and grow the fan-out every capture pays for.
    assert_eq!(
        super::captures_for(NEEDLE),
        0,
        "the capture bus kept a subscription after its handle dropped"
    );
}

#[test]
fn read_delimited_frames_a_split_delimiter() {
    const HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nX-Fixture: framed\r";
    const TAIL: &[u8] = b"\n\r\nTRAILING-BODY";
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    let mut server = Some(std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        std::io::Write::write_all(&mut stream, HEAD).unwrap();
        std::io::Write::flush(&mut stream).unwrap();
        // Two writes, with the delimiter split between them. No pause is needed
        // to make the reader resume across that split: it reads one byte per
        // syscall, so the overlap scan runs on every byte of every run whatever
        // TCP does with the segment boundary.
        std::io::Write::write_all(&mut stream, TAIL).unwrap();
        std::io::Write::flush(&mut stream).unwrap();
    }));

    let mut client = std::net::TcpStream::connect(address).unwrap();
    let framed = super::read_delimited(
        &mut client,
        b"\r\n\r\n",
        1024,
        std::time::Duration::from_secs(2),
    )
    .unwrap();

    assert_eq!(
        framed.as_ref(),
        b"HTTP/1.1 200 OK\r\nX-Fixture: framed\r\n\r\n"
    );
    // Framed, not drained: a reader that consumed past its delimiter would
    // leave the next read of this connection short.
    let rest = super::bounded_read(&mut client, std::time::Duration::from_secs(2), 1024).unwrap();
    assert_eq!(rest.as_ref(), b"TRAILING-BODY");
    super::join_thread_bounded(&mut server, std::time::Duration::from_secs(2)).unwrap();
}

#[test]
fn read_delimited_bounds_a_dribbling_peer_by_deadline() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_sender, stop_receiver) = std::sync::mpsc::channel::<()>();

    // One byte at a time, each inside the reader's bound. A reader that gave
    // every byte a fresh full timeout would never expire against this peer.
    let mut server = Some(std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        while stop_receiver.try_recv().is_err() {
            match std::io::Write::write_all(&mut stream, b"x") {
                Ok(()) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(_) => return,
            }
        }
    }));

    let mut client = std::net::TcpStream::connect(address).unwrap();
    let started = std::time::Instant::now();
    let result = super::read_delimited(
        &mut client,
        b"\r\n\r\n",
        64 * 1024,
        std::time::Duration::from_millis(200),
    );

    // The kind, not just the failure: a reset peer, a size limit, and an early
    // EOF all fail a test named for the deadline while proving nothing about it.
    let error = result.expect_err("a peer that never sent the delimiter produced a frame anyway");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::TimedOut,
        "the framed read failed for a reason other than its own deadline: {error}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the framed read outlived its own deadline"
    );
    stop_sender.send(()).unwrap();
    drop(client);
    super::join_thread_bounded(&mut server, std::time::Duration::from_secs(2)).unwrap();
}

/// Hold this process open until the parent kills it, reporting whether this
/// process was that child.
///
/// The parent's subject in every one of these cases is the kill: each proves the
/// guard reaps a child that will not exit on its own. So the child prints
/// `marker` for the parent's line wait, flushes it — a marker left in the buffer
/// reads as a child that never got there — and then parks on a channel whose
/// sender it holds itself, so only the parent can end it. Reaching the bound
/// means the parent never did, which is the fault under test.
fn hold_until_parent_kills(mode: &str, marker: &str) -> bool {
    if !super::is_private_child(mode) {
        return false;
    }
    println!("{marker}");
    std::io::Write::flush(&mut std::io::stdout())
        .expect("the held child could not flush its marker");
    let (_sender, receiver) = std::sync::mpsc::channel::<()>();
    let result = receiver.recv_timeout(std::time::Duration::from_secs(30));
    assert!(result.is_ok(), "fixture parent did not terminate child");
    true
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
