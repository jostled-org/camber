#[path = "support/deterministic.rs"]
pub mod deterministic;
#[path = "support/http.rs"]
pub mod http_support;
#[path = "support/process.rs"]
pub mod process_support;

#[path = "component_runtime_concurrency/async_channels.rs"]
mod async_channels;
#[path = "component_runtime_concurrency/async_tasks.rs"]
mod async_tasks;
#[path = "component_runtime_concurrency/periodic_scheduling.rs"]
mod periodic_scheduling;
#[path = "component_runtime_concurrency/schedule_probes.rs"]
mod schedule_probes;
#[path = "component_runtime_concurrency/shutdown_observation.rs"]
mod shutdown_observation;
#[path = "component_runtime_concurrency/sync_channels.rs"]
mod sync_channels;
#[path = "component_runtime_concurrency/task_cancellation.rs"]
mod task_cancellation;
#[path = "component_runtime_concurrency/task_lifecycle.rs"]
mod task_lifecycle;
#[path = "component_runtime_concurrency/task_racing.rs"]
mod task_racing;
#[path = "component_runtime_concurrency/test_runtime_macro.rs"]
mod test_runtime_macro;
#[path = "component_runtime_concurrency/watch_channels.rs"]
mod watch_channels;

use std::num::NonZeroUsize;

use deterministic::{DeterministicGenerator, STABLE_SEED};

#[test]
fn deterministic_cases_reproduce_the_documented_sequence() {
    let generator = DeterministicGenerator::stable();
    let upper = NonZeroUsize::new(17).unwrap();
    let mut case = generator.case(12);

    assert_eq!(generator.seed(), STABLE_SEED);
    assert_eq!(case.seed(), STABLE_SEED);
    assert_eq!(case.index(), 12);
    assert_eq!(case.bounded(upper), 7);
    assert!(!case.boolean());
    assert_eq!(
        case.select(&["route", "multipart", "origin", "retry"]),
        Some(&"retry")
    );
    assert_eq!(case.to_string(), "seed=0x43414d4245520007 case=12");
}

#[test]
fn deterministic_cases_replay_and_keep_all_outputs_bounded() {
    let generator = DeterministicGenerator::stable();
    let upper = NonZeroUsize::new(31).unwrap();
    let mut first = generator.case(91);
    let mut replay = generator.case(91);

    for _ in 0..128 {
        let first_value = first.bounded(upper);
        assert_eq!(first_value, replay.bounded(upper));
        assert!(first_value < upper.get());
    }

    assert_eq!(first.select::<u8>(&[]), None);
    assert_eq!(first.bounded(upper), replay.bounded(upper));
}
