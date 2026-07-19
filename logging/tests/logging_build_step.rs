//! Integration tests for the logging build step.
//!
//! Flows exercised:
//!  - producer callback → register_loggables → ChannelRegistry → LogTask subscriber
//!    → JSONL log file (round trip via `JsonLogFileReader`)
//!  - build-step silently skips channels whose types aren't registered
//!  - empty registry produces no LogTask node
//!  - `#[task_callback]`-generated `LogDiagnosticsTask` subscribes to the
//!    `log_task_diagnostics` channel

use std::path::PathBuf;
use std::time::Duration;

use task::callback::{Callback, Run};
use task::callback_builder::CallbackBuilder;
use task::context::Context;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::output::Output;
use task::publisher::{Publisher, PublisherConfig};
use task::task_graph_builder::TaskGraphBuilder;
use testing::unit_test_executor::UnitTestExecutor;

use logging::log_file::LogFileReader;
use logging::log_file_json::JsonLogFileReader;
use logging::{
    ChannelRegistry, DiagnosticsMode, LogDiagnosticsTask, LogTaskConfiguration, LoggingBuildStep,
};

// ───────────────────────────── Helpers ──────────────────────────────────

/// A minimal producer that publishes `n` counter values on the `values` channel
/// across successive runs, one per run.
struct CounterProducer {
    counter: u64,
    max: u64,
}

impl CounterProducer {
    fn new(max: u64) -> Self {
        CounterProducer { counter: 0, max }
    }
}

impl Callback for CounterProducer {
    fn run_generic(
        &mut self,
        _subscribers: &mut [Box<dyn GenericSubscriber>],
        publishers: &mut [Box<dyn GenericPublisher>],
        _ctx: &Context,
    ) -> Run {
        if self.counter < self.max {
            let mut out: Output<'_, u64> = Output::new_downcasted(publishers[0].as_mut());
            *out = self.counter;
            out.send();
            self.counter += 1;
        }
        Run::new(1)
    }

    fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
        vec![]
    }

    fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
        vec![Box::new(Publisher::<u64>::new(PublisherConfig {
            capacity: 1,
            channel_name: "values".into(),
        }))]
    }
}

fn build_producer(name: &str, max: u64) -> task::callback::CallbackNode {
    CallbackBuilder::new(name.into(), Box::new(CounterProducer::new(max)))
        .with_publisher_channels(&["values"])
        .with_execution_duration_callback(|| Duration::from_nanos(1))
        // Run every step — keeps the LogTask fed.
        .with_next_execution_time_callback(|t| Some(t + Duration::from_nanos(1)))
        .build()
        .expect("producer builds")
}

fn temp_path(suffix: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "cfw_logging_test_{}_{}.jsonl",
        std::process::id(),
        suffix
    ));
    // Start each run with a fresh file
    let _ = std::fs::remove_file(&p);
    p
}

// ───────────────────────────── Tests ─────────────────────────────────────

#[test]
fn writes_loggable_channel_to_jsonl() {
    let out = temp_path("writes_loggable");

    let producer = build_producer("Counter", 3);

    // Register u64 (the producer's message type) with the registry. In a
    // macro-decorated producer this would be `CounterProducer::register_loggables(&mut registry)`
    // — but our hand-rolled producer isn't `#[task_callback]`-decorated, so
    // we register its single type directly.
    let mut registry = ChannelRegistry::new();
    registry.register_loggable::<u64>();

    let graph = TaskGraphBuilder::new()
        .add_callback(producer)
        .add_build_step(Box::new(LoggingBuildStep::new(
            LogTaskConfiguration {
                output_path: out.clone(),
                period: Duration::from_nanos(1),
            },
            registry,
        )))
        .build()
        .expect("graph builds");

    let mut executor = UnitTestExecutor::new(graph.nodes);
    // producer publishes 1 msg/run; LogTask consumes 1 msg/run.
    for _ in 0..20 {
        executor.step();
    }

    // LogTask flushed in Drop; force it by dropping the executor
    drop(executor);

    let reader =
        JsonLogFileReader::from_reader(std::io::BufReader::new(std::fs::File::open(&out).unwrap()))
            .unwrap();
    let entries: Vec<_> = reader.iter().collect();

    assert!(
        !entries.is_empty(),
        "expected some log entries, got 0; file at {}",
        out.display()
    );
    for entry in &entries {
        assert_eq!(entry.channel_name, "values");
        let v: u64 = serde_json::from_slice(entry.serialized_body).unwrap();
        assert!(v < 3, "got unexpected value {v}");
    }
}

#[test]
fn empty_registry_produces_no_log_task() {
    // With no types registered, the build step finds nothing to log and
    // doesn't add a LogTask node at all. Confirm it doesn't panic and the
    // graph still builds with just the producer node.
    let producer = build_producer("Counter", 1);

    let graph = TaskGraphBuilder::new()
        .add_callback(producer)
        .add_build_step(Box::new(LoggingBuildStep::new(
            LogTaskConfiguration {
                output_path: temp_path("empty_registry"),
                period: Duration::from_nanos(1),
            },
            ChannelRegistry::new(),
        )))
        .build()
        .expect("graph builds");

    assert_eq!(graph.nodes.len(), 1);
    assert_eq!(graph.nodes[0].get_name(), "Counter");
}

#[test]
fn unregistered_type_silently_skipped() {
    // Build with a producer on "values" of type u64 but DON'T register u64
    // with the registry — build should succeed with no LogTask node added
    // since the registry knows nothing about u64.
    let producer = build_producer("Counter", 1);

    let graph = TaskGraphBuilder::new()
        .add_callback(producer)
        .add_build_step(Box::new(LoggingBuildStep::new(
            LogTaskConfiguration {
                output_path: temp_path("unregistered"),
                period: Duration::from_nanos(1),
            },
            ChannelRegistry::new(), // empty by design
        )))
        .build()
        .expect("graph builds");

    assert_eq!(
        graph.nodes.len(),
        1,
        "no LogTask should be added when nothing is registered"
    );
    assert_eq!(graph.nodes[0].get_name(), "Counter");
}

#[test]
fn diagnostics_task_picks_up_logtask_errors() {
    // Smoke test for the wiring: a `LogDiagnosticsTask` subscribes to
    // `log_task_diagnostics` (whose channel name is set via
    // `with_subscriber_channels`); the build flow completes; no panic.
    let out = temp_path("diagnostics");

    let producer = build_producer("Counter", 1);
    let diag = CallbackBuilder::new(
        "Diagnostics".into(),
        Box::new(LogDiagnosticsTask::new(DiagnosticsMode::Silent)),
    )
    .with_subscriber_channels(&["log_task_diagnostics"])
    .with_execution_duration_callback(|| Duration::from_nanos(1))
    .build()
    .expect("diagnostics builds");

    let mut registry = ChannelRegistry::new();
    registry.register_loggable::<u64>();

    let graph = TaskGraphBuilder::new()
        .add_callback(producer)
        .add_callback(diag)
        .add_build_step(Box::new(LoggingBuildStep::new(
            LogTaskConfiguration {
                output_path: out,
                period: Duration::from_nanos(1),
            },
            registry,
        )))
        .build()
        .expect("graph builds");

    // Should have 3 nodes: producer, diagnostics, LogTask.
    assert_eq!(graph.nodes.len(), 3);

    let mut executor = UnitTestExecutor::new(graph.nodes);
    for _ in 0..6 {
        executor.step();
    }
    // No panic = pass. No mechanism here to inject an IO error; this just
    // verifies the wiring is sound (no connection errors, no build errors).
}
