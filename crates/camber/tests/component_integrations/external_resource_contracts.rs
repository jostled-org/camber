use crate::resources::ExternalRun;

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
}
