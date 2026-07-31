use std::path::Path;
use std::time::Duration;

use exact_replay_executor::{ExactReplayConfig, ExactReplayExecutor};
use fizz_buzz_application::build_exact_replay_graph;
use task::executor::Executor;

/// Replays the committed sample log end to end and expects every recorded
/// execution to reproduce its logged outputs with no divergence.
#[test]
#[cfg_attr(miri, ignore)] // replaying the full sample log is prohibitively slow under Miri
fn exact_replays_sample_log_without_divergence() {
    // nextest runs tests from a scratch directory, so resolve the resource
    // relative to the crate manifest rather than the process CWD.
    let resource = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/fizz-buzz-log.ndjson");
    let graph = build_exact_replay_graph(&resource).expect("build graph");

    let config = ExactReplayConfig::new(graph.nodes, graph.registry, graph.log_reader);
    let mut executor = ExactReplayExecutor::new(config).expect("construct executor");

    assert!(executor.execution_count() > 0, "sample log has executions");

    executor.start();

    let deadline =
        std::time::Instant::now() + Duration::from_secs(if cfg!(miri) { 120 } else { 10 });
    while executor.is_running() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !executor.is_running(),
        "replay did not finish within deadline"
    );

    let stop_result = executor.stop();
    assert!(stop_result.is_ok(), "stop failed: {stop_result:?}");

    let replay_errors = executor.replay_errors();
    assert!(
        replay_errors.is_empty(),
        "expected clean replay, got errors: {replay_errors:?}"
    );
    assert_eq!(
        executor.consumed_count(),
        executor.execution_count(),
        "every logged execution should have been consumed"
    );
}
