//! Integration tests for the logging build step.
//!
//! Flows exercised:
//!  - producer callback → auto-registered channel → ChannelRegistry → LogTask
//!    subscriber → JSONL log file (round trip via `JsonLogFileReader`)
//!  - build-step silently skips channels whose types aren't loggable
//!  - `with_unlogged_channels` denylists loggable channels
//!  - `LogDiagnosticsTask` subscribes to one diagnostics channel per `LogTask`
//!  - channels split across multiple `LogTask`s that share one log file

use std::path::PathBuf;
use std::time::Duration;

use task::callback::{Callback, CallbackViews, PortMut, Run};
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
    DiagnosticsMode, LogDiagnosticsTask, LogTaskConfiguration, LoggingBuildStep,
    log_task_diagnostics_channel, log_task_name,
};

// ───────────────────────────── Helpers ──────────────────────────────────

/// A minimal producer that publishes `n` counter values on the `values` channel
/// across successive runs, one per run.
struct CounterProducer {
    publisher: Publisher<u64>,
    counter: u64,
    max: u64,
}

impl CounterProducer {
    fn new(max: u64) -> Self {
        CounterProducer {
            publisher: Publisher::<u64>::new(PublisherConfig {
                capacity: 1,
                channel_name: String::new(),
            }),
            counter: 0,
            max,
        }
    }
}

impl Callback for CounterProducer {
    fn run(&mut self, _ctx: &Context) -> Run {
        if self.counter < self.max {
            let mut out = Output::<u64>::new_default(&mut self.publisher);
            *out = self.counter;
            out.send();
            self.counter += 1;
        }
        Run::new(1)
    }

    fn register_channels(&self, registry: &mut task::channel_registry::ChannelRegistry) {
        // Hand-written callbacks register their concrete port types explicitly;
        // `#[task_callback]` does this for you.
        task::channel_registry::Probe::<u64>::new().try_register(registry);
        task::channel_registry::Probe::<u64>::new()
            .try_register_channel(registry, self.publisher.config().channel_name.clone());
    }

    fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
    fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
        f(&self.publisher);
    }
    fn for_each_subscriber_mut<'a>(
        &'a mut self,
        _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
    ) {
    }
    fn for_each_publisher_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericPublisher)) {
        f(&mut self.publisher);
    }
    fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
        f(PortMut::Publisher(&mut self.publisher));
    }
}

fn build_producer_on(name: &str, channel: &str, max: u64) -> task::callback::CallbackNode {
    CallbackBuilder::new(name.into(), Box::new(CounterProducer::new(max)))
        .with_publisher_channels(&[channel])
        .with_execution_duration_callback(|| Duration::from_nanos(1))
        // Run every step — keeps the LogTask fed.
        .with_next_execution_time_callback(|t| Some(t + Duration::from_nanos(1)))
        .build()
        .expect("producer builds")
}

fn build_producer(name: &str, max: u64) -> task::callback::CallbackNode {
    build_producer_on(name, "values", max)
}

/// A payload type that deliberately does NOT implement `Loggable` (no serde
/// derives), so its channel is never registered and always skipped by the
/// logging build step.
#[derive(Default)]
struct NonLoggable {
    _value: u32,
}

/// A minimal producer publishing `NonLoggable` values on the `opaque` channel.
struct NonLoggableProducer {
    publisher: Publisher<NonLoggable>,
    counter: u32,
}

impl NonLoggableProducer {
    fn new() -> Self {
        NonLoggableProducer {
            publisher: Publisher::<NonLoggable>::new(PublisherConfig {
                capacity: 1,
                channel_name: String::new(),
            }),
            counter: 0,
        }
    }
}

impl Callback for NonLoggableProducer {
    fn run(&mut self, _ctx: &Context) -> Run {
        if self.counter < 1 {
            let mut out = Output::<NonLoggable>::new_default(&mut self.publisher);
            out._value = self.counter;
            out.send();
            self.counter += 1;
        }
        Run::new(1)
    }

    fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
    fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
        f(&self.publisher);
    }
    fn for_each_subscriber_mut<'a>(
        &'a mut self,
        _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
    ) {
    }
    fn for_each_publisher_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericPublisher)) {
        f(&mut self.publisher);
    }
    fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
        f(PortMut::Publisher(&mut self.publisher));
    }
}

fn build_non_loggable_producer(name: &str) -> task::callback::CallbackNode {
    CallbackBuilder::new(name.into(), Box::new(NonLoggableProducer::new()))
        .with_publisher_channels(&["opaque"])
        .with_execution_duration_callback(|| Duration::from_nanos(1))
        .with_next_execution_time_callback(|t| Some(t + Duration::from_nanos(1)))
        .build()
        .expect("non-loggable producer builds")
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
#[cfg_attr(not(miri), ignore = "TODO Need to handle execution log descriptor")]
#[cfg_attr(miri, ignore = "Miri doesn't support adding/removing files")]
fn writes_loggable_channel_to_jsonl() {
    let out = temp_path("writes_loggable");

    let producer = build_producer("Counter", 3);

    let mut graph = TaskGraphBuilder::new()
        .add_pool(1, |p| p.add_callback(producer))
        .add_build_step(Box::new(LoggingBuildStep::new(LogTaskConfiguration {
            output_path: out.clone(),
            period: Duration::from_nanos(1),
            num_tasks: 1,
        })))
        .build()
        .expect("graph builds");

    let mut executor = UnitTestExecutor::new(graph.pools.remove(0).nodes);
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

    assert_eq!(
        entries.len(),
        3,
        "all 3 published messages should be logged; file at {}",
        out.display()
    );
    for entry in &entries {
        assert_eq!(entry.channel_name, "values");
        let v: u64 = serde_json::from_slice(entry.serialized_body).unwrap();
        assert!(v < 3, "got unexpected value {v}");
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri doesn't support adding/removing files")]
fn non_loggable_channel_silently_skipped() {
    // The producer's payload type doesn't implement Loggable, and the
    // framework's execution-log channel is denylisted, so there are no
    // loggable channels at all — no LogTask node is added and the build
    // still succeeds.
    let producer = build_non_loggable_producer("OpaqueProducer");

    let graph = TaskGraphBuilder::new()
        .add_pool(1, |p| p.add_callback(producer))
        .add_build_step(Box::new(
            LoggingBuildStep::new(LogTaskConfiguration {
                output_path: temp_path("non_loggable"),
                period: Duration::from_nanos(1),
                num_tasks: 1,
            })
            .with_unlogged_channels([task::execution_log::EXECUTION_LOG_CHANNEL]),
        ))
        .build()
        .expect("graph builds");

    assert_eq!(
        graph.pools[0].nodes.len(),
        1,
        "no LogTask should be added when nothing is loggable"
    );
    assert_eq!(
        graph.pools[0].nodes[0].with_exclusive(|n| n.name().to_owned()),
        "OpaqueProducer"
    );
}

#[test]
#[cfg_attr(miri, ignore = "Miri doesn't support adding/removing files")]
fn denylisted_channel_silently_skipped() {
    // `values` is loggable and logged; `secret` is loggable but denylisted,
    // so it must not appear in the log. The execution-log channel is also
    // denylisted to keep this test focused on the two producer channels.
    let out = temp_path("denylisted");

    let mut graph = TaskGraphBuilder::new()
        .add_pool(1, |p| {
            p.add_callback(build_producer_on("Counter", "values", 3))
                .add_callback(build_producer_on("SecretProducer", "secret", 3))
        })
        .add_build_step(Box::new(
            LoggingBuildStep::new(LogTaskConfiguration {
                output_path: out.clone(),
                period: Duration::from_nanos(1),
                num_tasks: 1,
            })
            .with_unlogged_channels([
                "secret".to_owned(),
                task::execution_log::EXECUTION_LOG_CHANNEL.into(),
            ]),
        ))
        .build()
        .expect("graph builds");

    let mut executor = UnitTestExecutor::new(graph.pools.remove(0).nodes);
    for _ in 0..20 {
        executor.step();
    }
    drop(executor);

    let reader =
        JsonLogFileReader::from_reader(std::io::BufReader::new(std::fs::File::open(&out).unwrap()))
            .unwrap();
    let entries: Vec<_> = reader.iter().collect();
    assert!(
        !entries.is_empty(),
        "logged channels should produce entries"
    );
    for entry in &entries {
        assert_eq!(
            entry.channel_name, "values",
            "denylisted 'secret' channel must not be logged"
        );
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri doesn't support adding/removing files")]
fn diagnostics_task_picks_up_logtask_errors() {
    // Smoke test for the wiring: a `LogDiagnosticsTask` subscribes to the
    // `LogTask[0]_diagnostics` channel; the build flow completes; no panic.
    let out = temp_path("diagnostics");

    let producer = build_producer("Counter", 1);
    let diag = CallbackBuilder::new(
        "Diagnostics".into(),
        Box::new(LogDiagnosticsTask::for_log_tasks(
            DiagnosticsMode::Silent,
            1,
        )),
    )
    .with_execution_duration_callback(|| Duration::from_nanos(1))
    .build()
    .expect("diagnostics builds");

    let mut graph = TaskGraphBuilder::new()
        .add_pool(1, |p| p.add_callback(producer).add_callback(diag))
        .add_build_step(Box::new(LoggingBuildStep::new(LogTaskConfiguration {
            output_path: out,
            period: Duration::from_nanos(1),
            num_tasks: 1,
        })))
        .build()
        .expect("graph builds");

    // Should have 3 nodes: producer, diagnostics, LogTask[0].
    assert_eq!(graph.pools[0].nodes.len(), 3);
    assert_eq!(
        graph.pools[0].nodes[2].with_exclusive(|n| n.name().to_owned()),
        log_task_name(0)
    );

    let mut executor = UnitTestExecutor::new(graph.pools.remove(0).nodes);
    for _ in 0..6 {
        executor.step();
    }
    // No panic = pass. No mechanism here to inject an IO error; this just
    // verifies the wiring is sound (no connection errors, no build errors).
}

#[test]
#[cfg_attr(not(miri), ignore = "TODO Need to handle execution log descriptor")]
#[cfg_attr(miri, ignore = "Miri doesn't support adding/removing files")]
fn splits_channels_across_multiple_log_tasks_sharing_one_file() {
    // Four channels on four producers, logged by two LogTasks. Both tasks
    // write to the same file; every channel's messages must appear exactly
    // once (no channel logged by two tasks, none dropped).
    let out = temp_path("split_shared_file");

    let channels = ["ch0", "ch1", "ch2", "ch3"];
    const MESSAGES_PER_CHANNEL: u64 = 3;

    let mut graph = TaskGraphBuilder::new()
        .add_pool(1, |mut p| {
            for (i, channel) in channels.iter().enumerate() {
                p = p.add_callback(build_producer_on(
                    &format!("Producer{i}"),
                    channel,
                    MESSAGES_PER_CHANNEL,
                ));
            }
            p
        })
        .add_build_step(Box::new(LoggingBuildStep::new(LogTaskConfiguration {
            output_path: out.clone(),
            period: Duration::from_nanos(1),
            num_tasks: 2,
        })))
        .build()
        .expect("graph builds");

    // 4 producers + 2 log tasks.
    assert_eq!(graph.pools[0].nodes.len(), 6);
    assert_eq!(
        graph.pools[0].nodes[4].with_exclusive(|n| n.name().to_owned()),
        log_task_name(0)
    );
    assert_eq!(
        graph.pools[0].nodes[5].with_exclusive(|n| n.name().to_owned()),
        log_task_name(1)
    );

    // Each log task exposes its own diagnostics channel.
    assert_eq!(
        graph.pools[0].nodes[4].with_exclusive(|n| n.callback().collect_publishers()[0]
            .config()
            .channel_name
            .clone()),
        log_task_diagnostics_channel(0)
    );
    assert_eq!(
        graph.pools[0].nodes[5].with_exclusive(|n| n.callback().collect_publishers()[0]
            .config()
            .channel_name
            .clone()),
        log_task_diagnostics_channel(1)
    );

    let mut executor = UnitTestExecutor::new(graph.pools.remove(0).nodes);
    for _ in 0..20 {
        executor.step();
    }
    drop(executor);

    let reader =
        JsonLogFileReader::from_reader(std::io::BufReader::new(std::fs::File::open(&out).unwrap()))
            .unwrap();

    let mut per_channel_counts = std::collections::HashMap::new();
    for entry in reader.iter() {
        *per_channel_counts
            .entry(entry.channel_name.to_owned())
            .or_insert(0) += 1;
        let v: u64 = serde_json::from_slice(entry.serialized_body).unwrap();
        assert!(v < MESSAGES_PER_CHANNEL, "got unexpected value {v}");
    }

    assert_eq!(
        per_channel_counts.len(),
        channels.len(),
        "expected entries from every channel, got {per_channel_counts:?}"
    );
    for channel in channels {
        assert_eq!(
            per_channel_counts.get(channel).copied().unwrap_or(0),
            MESSAGES_PER_CHANNEL as usize,
            "channel '{channel}' must be logged exactly once per message (no shard overlap)"
        );
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri doesn't support adding/removing files")]
fn num_tasks_clamped_to_channel_count() {
    // Requesting more log tasks than there are loggable channels must not
    // produce empty LogTask nodes. One producer channel plus the framework's
    // execution-log channel = two loggers, so num_tasks clamps to two.
    let producer = build_producer("Counter", 1);

    let graph = TaskGraphBuilder::new()
        .add_pool(1, |p| p.add_callback(producer))
        .add_build_step(Box::new(LoggingBuildStep::new(LogTaskConfiguration {
            output_path: temp_path("clamped"),
            period: Duration::from_nanos(1),
            num_tasks: 10,
        })))
        .build()
        .expect("graph builds");

    assert_eq!(
        graph.pools[0].nodes.len(),
        3,
        "two loggable channels → two LogTask nodes"
    );
    assert_eq!(
        graph.pools[0].nodes[1].with_exclusive(|n| n.name().to_owned()),
        log_task_name(0)
    );
    assert_eq!(
        graph.pools[0].nodes[2].with_exclusive(|n| n.name().to_owned()),
        log_task_name(1)
    );
}

#[test]
#[cfg_attr(miri, ignore = "Miri doesn't support adding/removing files")]
fn diagnostics_task_subscribes_to_every_log_task() {
    // With two log tasks, the diagnostics task must build one subscriber per
    // per-task diagnostics channel.
    let out = temp_path("multi_diagnostics");

    let diag = CallbackBuilder::new(
        "Diagnostics".into(),
        Box::new(LogDiagnosticsTask::for_log_tasks(
            DiagnosticsMode::Silent,
            2,
        )),
    )
    .with_execution_duration_callback(|| Duration::from_nanos(1))
    .build()
    .expect("diagnostics builds");

    let mut graph = TaskGraphBuilder::new()
        .add_pool(1, |p| {
            p.add_callback(build_producer_on("ProducerA", "ch_a", 1))
                .add_callback(build_producer_on("ProducerB", "ch_b", 1))
                .add_callback(diag)
        })
        .add_build_step(Box::new(LoggingBuildStep::new(LogTaskConfiguration {
            output_path: out,
            period: Duration::from_nanos(1),
            num_tasks: 2,
        })))
        .build()
        .expect("graph builds");

    // 2 producers + diagnostics + 2 log tasks.
    assert_eq!(graph.pools[0].nodes.len(), 5);

    let subscribed_channels: Vec<String> = graph.pools[0].nodes[2].with_exclusive(|n| {
        n.callback()
            .collect_subscribers()
            .iter()
            .map(|s| s.config().channel_name.to_owned())
            .collect()
    });
    assert_eq!(
        subscribed_channels,
        vec![
            log_task_diagnostics_channel(0),
            log_task_diagnostics_channel(1)
        ],
        "diagnostics task must subscribe to both log tasks diagnostic's channels"
    );

    let _executor = UnitTestExecutor::new(graph.pools.remove(0).nodes);
    for _ in 0..6 {}
    // No panic = pass: both diagnostics channels connected to their publishers.
}
