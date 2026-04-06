use metrics_exporter_prometheus::PrometheusBuilder;

#[test]
fn init_metrics_returns_handle() {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder).ok();

    // Record a counter so there is something to render.
    metrics::counter!("test_requests_total").increment(1);

    let output = handle.render();
    assert!(
        output.contains("test_requests_total"),
        "expected test_requests_total in Prometheus output, got: {output}"
    );
}
