use std::time::Duration;

use camber_bench::load::{BenchResult, LoadGenerator, load_command, three_phase_bench_with};

#[test]
fn production_commands_select_supported_output_and_duration() {
    let url = "http://127.0.0.1:12345/";
    for (generator, expected) in [
        (
            LoadGenerator::Oha,
            vec![
                "--output-format",
                "json",
                "-z",
                "1s",
                "-c",
                "4",
                "--no-tui",
                url,
            ],
        ),
        (
            LoadGenerator::Wrk,
            vec!["-t2", "-d", "1s", "-c", "4", "--latency", url],
        ),
    ] {
        let command = load_command(generator, url, 4, Duration::ZERO);
        assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
    }
}

fn result(value: f64) -> BenchResult {
    BenchResult {
        req_per_sec: value,
        latency_avg_ms: 1.0,
        latency_p50_ms: 1.0,
        latency_p90_ms: 1.0,
        latency_p99_ms: 1.0,
        error_count: 0,
    }
}

#[test]
fn production_phases_discard_warmups_and_keep_each_measurement() {
    let mut calls = Vec::new();
    let mut sleeps = Vec::new();
    let results = three_phase_bench_with(
        &[4, 16],
        Duration::from_secs(7),
        |connections, duration| {
            calls.push((connections, duration.as_secs()));
            Ok(result(calls.len() as f64))
        },
        |duration| sleeps.push(duration),
    )
    .unwrap();
    assert_eq!(calls, [(8, 5), (16, 7), (4, 7), (16, 7)]);
    assert_eq!(sleeps, [Duration::from_secs(2); 2]);
    assert_eq!(
        results
            .iter()
            .map(|(c, r)| (*c, r.req_per_sec))
            .collect::<Vec<_>>(),
        [(4, 3.0), (16, 4.0)]
    );
}

#[test]
fn production_phases_stop_at_each_failed_phase() {
    for failure in 1..=4 {
        let mut calls = 0;
        let outcome = three_phase_bench_with(
            &[4, 16],
            Duration::from_secs(7),
            |_, _| {
                calls += 1;
                match calls == failure {
                    true => Err(camber_bench::error::BenchError::LoadGenerator(
                        "fixture failure".into(),
                    )),
                    false => Ok(result(1.0)),
                }
            },
            |_| {},
        );
        assert!(outcome.is_err());
        assert_eq!(calls, failure);
    }
}

#[test]
fn reports_preserve_fixture_frameworks_and_concurrency() {
    use camber_bench::report::{BenchmarkRun, ConcurrencyResult, FrameworkRun};
    let runs: Vec<_> = ["hello_text", "hello_json", "path_param", "static_file"]
        .into_iter()
        .map(|name| BenchmarkRun {
            name: name.into(),
            frameworks: ["fixture-a", "fixture-b"]
                .into_iter()
                .map(|framework| FrameworkRun {
                    framework: framework.into(),
                    results: [4, 16]
                        .into_iter()
                        .map(|concurrency| ConcurrencyResult {
                            concurrency,
                            result: result(f64::from(concurrency)),
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();
    let json = camber_bench::report::format_json(&runs).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap(),
        serde_json::to_value(&runs).unwrap()
    );
    let markdown = camber_bench::report::format_markdown(&runs);
    for run in &runs {
        assert!(markdown.contains(run.name.as_ref()));
        assert_eq!(run.framework_run("fixture-b").unwrap().results.len(), 2);
    }
    for expected in ["fixture-a", "fixture-b", "| 4 |", "| 16 |"] {
        assert!(markdown.contains(expected));
    }
}
