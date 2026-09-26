use std::time::Duration;

use crate::common::run_in_child;

const BOUND: Duration = Duration::from_secs(20);

#[test]
fn shutdown_flushes_spans_without_retrying_rejected_exports() {
    run_in_child(
        "otlp_export::shutdown_flushes_spans_without_retrying_rejected_exports",
        "otlp-single-attempt",
        "OTLP_SINGLE_ATTEMPT_COMPLETE",
        BOUND,
        || assert_export_attempts("14"),
    );
}

#[test]
fn shutdown_flushes_spans_to_the_configured_collector() {
    run_in_child(
        "otlp_export::shutdown_flushes_spans_to_the_configured_collector",
        "otlp-successful-export",
        "OTLP_SUCCESSFUL_EXPORT_COMPLETE",
        BOUND,
        || assert_export_attempts("0"),
    );
}

fn assert_export_attempts(status: &'static str) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let collector = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(BOUND, collect_exports(listener, stopped, status))
                    .await
                    .expect("collector did not settle")
            })
    });
    let result = camber::runtime::builder()
        .worker_threads(2)
        .otel_endpoint(&endpoint)
        .run(|| {
            let span = camber::tracing::info_span!("dependency_export_probe");
            drop(span.enter());
        });
    stop.send(())
        .expect("collector stopped before runtime shutdown");
    let attempts = collector.join().expect("collector panicked");
    result.expect("runtime did not shut down cleanly");
    assert_eq!(
        attempts, 1,
        "shutdown must flush one batch with one attempt"
    );
}

async fn collect_exports(
    listener: std::net::TcpListener,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
    status: &'static str,
) -> usize {
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let socket = tokio::select! {
        accepted = listener.accept() => accepted.unwrap().0,
        _ = &mut stopped => return 0,
    };
    let mut connection = h2::server::handshake(socket).await.unwrap();
    let mut attempts = 0;
    loop {
        let request = tokio::select! {
            request = connection.accept() => request,
            _ = &mut stopped => return attempts,
        };
        match request {
            Some(Ok((request, response))) => {
                assert_eq!(
                    request.uri().path(),
                    "/opentelemetry.proto.collector.trace.v1.TraceService/Export"
                );
                respond_to_export(response, status);
                attempts += 1;
            }
            Some(Err(error)) => panic!("collector connection failed: {error}"),
            None => {
                drop(stopped.await);
                return attempts;
            }
        }
    }
}

fn respond_to_export(mut response: h2::server::SendResponse<bytes::Bytes>, status: &'static str) {
    let headers = http::Response::builder()
        .header("content-type", "application/grpc")
        .body(())
        .unwrap();
    let mut stream = response.send_response(headers, false).unwrap();
    if status == "0" {
        stream
            .send_data(bytes::Bytes::from_static(&[0, 0, 0, 0, 0]), false)
            .unwrap();
    }
    let mut trailers = http::HeaderMap::new();
    trailers.insert("grpc-status", http::HeaderValue::from_static(status));
    stream.send_trailers(trailers).unwrap();
}
