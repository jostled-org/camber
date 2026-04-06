mod common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

/// A tracing layer that captures formatted event fields into a shared vec.
struct CapturingLayer {
    captured: Arc<Mutex<Vec<String>>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturingLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = FieldCapture(String::new());
        event.record(&mut visitor);
        let mut guard = self.captured.lock().unwrap_or_else(|e| e.into_inner());
        guard.push(visitor.0);
    }
}

struct FieldCapture(String);

impl tracing::field::Visit for FieldCapture {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(&format!("{}={:?}", field.name(), value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(&format!("{}={}", field.name(), value));
    }

    fn record_u128(&mut self, field: &tracing::field::Field, value: u128) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(&format!("{}={}", field.name(), value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(&format!("{}={}", field.name(), value));
    }
}

#[test]
fn tracing_logs_request_lifecycle() {
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let layer = CapturingLayer {
        captured: Arc::clone(&captured),
    };

    common::test_runtime()
        .with_tracing()
        .run(|| {
            // Install our capturing subscriber as the custom subscriber
            let subscriber = tracing_subscriber::registry().with(layer);
            tracing::subscriber::set_global_default(subscriber).ok();

            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hello")
            });

            let addr = common::spawn_server(router);

            let resp = common::block_on(http::get(&format!("http://{addr}/hello"))).unwrap();
            assert_eq!(resp.status(), 200);

            runtime::request_shutdown();
        })
        .unwrap();

    let events = captured.lock().unwrap_or_else(|e| e.into_inner());
    let combined = events.join(" ");

    assert!(
        combined.contains("method=GET") || combined.contains("method=\"GET\""),
        "expected method=GET in tracing output, got: {combined}"
    );
    assert!(
        combined.contains("path=/hello") || combined.contains("path=\"/hello\""),
        "expected path=/hello in tracing output, got: {combined}"
    );
    assert!(
        combined.contains("status=200"),
        "expected status=200 in tracing output, got: {combined}"
    );
    assert!(
        combined.contains("latency_ms="),
        "expected latency_ms in tracing output, got: {combined}"
    );
}

#[camber::test]
async fn tracing_disabled_by_default() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "hello")
    });

    let addr = common::spawn_server(router);

    // Should work fine without tracing — no panic, no output
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello");

    runtime::request_shutdown();
}
