use crate::resources::ExternalRun;

/// The longest stream name the JetStream server accepts, stated here apart
/// from the owner's own bound so a wrong bound fails this test.
const JETSTREAM_STREAM_NAME_LIMIT: usize = 255;

#[test]
fn external_lane_cleanup_uses_unique_resource_names() {
    let first = ExternalRun::parse("run-a").expect("first run ID");
    let second = ExternalRun::parse("run-b").expect("second run ID");
    let runner_sized =
        ExternalRun::parse("0123456789abcdefghijklmnopqrstuv").expect("runner-sized run ID");

    assert_eq!(
        &*first.nats_subject("pubsub"),
        "camber.test.pubsub.72756e2d61"
    );
    assert_eq!(
        &*first.nats_queue_group("queue"),
        "camber-workers-queue-72756e2d61"
    );
    assert_eq!(
        &*first.dns_subdomain("example.com").expect("DNS subdomain"),
        "camber-72756e2d61.example.com"
    );
    assert_ne!(first.nats_subject("pubsub"), second.nats_subject("pubsub"));
    assert_ne!(first.nats_subject("pubsub"), first.nats_subject("queue"));
    assert_ne!(
        first.dns_subdomain("example.com").expect("first domain"),
        second.dns_subdomain("example.com").expect("second domain")
    );
    let runner_domain = runner_sized
        .dns_subdomain("example.com")
        .expect("runner-sized DNS subdomain");
    let run_labels = runner_domain
        .strip_suffix(".example.com")
        .expect("base domain suffix");
    assert_eq!(run_labels.split('.').count(), 2);
    assert!(run_labels.split('.').all(|label| label.len() <= 63));
    assert!(ExternalRun::parse("").is_err());
    assert!(ExternalRun::parse("invalid.run").is_err());
    jetstream_streams_are_unique_and_valid();
}

/// A run's JetStream stream names differ by run and purpose, fit the maximum
/// run ID, accept a name of exactly the server's limit and refuse one byte
/// more, and refuse every character a stream name cannot carry.
fn jetstream_streams_are_unique_and_valid() {
    let first = ExternalRun::parse("run-a").expect("first run ID");
    let second = ExternalRun::parse("run_B").expect("second run ID");
    let longest = ExternalRun::parse(&"r".repeat(64)).expect("maximum-length run ID");

    let stored = first.nats_stream("stored").expect("first stream");
    assert_eq!(&*stored, "camber-stored-run-a");
    assert_eq!(
        &*second.nats_stream("stored").expect("second stream"),
        "camber-stored-run_B"
    );
    assert_ne!(stored, first.nats_stream("other").expect("second purpose"));

    // `camber-<purpose>-<run>`: everything but the purpose.
    let framing = "camber-".len() + "-".len() + longest.run_id().len();
    let edge = JETSTREAM_STREAM_NAME_LIMIT - framing;
    let long = longest
        .nats_stream(&"p".repeat(edge))
        .expect("stream name at the server's limit");
    assert_eq!(long.len(), JETSTREAM_STREAM_NAME_LIMIT);
    assert!(long.ends_with(&"r".repeat(64)));
    assert!(
        long.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    );
    assert!(
        longest.nats_stream(&"p".repeat(edge + 1)).is_err(),
        "a stream name one byte over the server's limit must be refused"
    );

    for forbidden in [
        "",
        "dot.ted",
        "wild*",
        "tail>",
        "sp ace",
        "sl/ash",
        "back\\slash",
        "tab\t",
        "ünï",
    ] {
        assert!(
            first.nats_stream(forbidden).is_err(),
            "stream purpose {forbidden:?} must be refused"
        );
    }
}
