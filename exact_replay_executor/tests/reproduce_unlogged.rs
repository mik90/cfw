//! End-to-end tests for reproducing unlogged intermediate values.
//!
//! A three-node chain: a producer publishes on a *source* channel, a middle
//! node transforms it onto an *output* channel, and a collector consumes the
//! output. When the logging build step logs only the output channel (e.g. it
//! only has a serializer for `String`, not `u64`), the source channel's
//! payloads are absent from the ordinary log. Replay must reproduce them by
//! re-running the producing node and feed the captured output to the middle
//! node's hydration.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use task::callback::CallbackNode;
use task::callback_builder::CallbackBuilder;
use task::channel_registry::ChannelRegistry;
use task::execution_log::{
    Direction, EXECUTION_LOG_CHANNEL, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, ExecutionLogDescriptor,
    ExecutionLogEntry, ExecutionLogMessage, LoggedMessage,
};
use task::executor::{Executor, ExecutorStopSignal, ThreadPoolConfig};
use task::input::InputSpan;
use task::loggable::Loggable;
use task::message::MessageHeader;
use task::output::Output;
use task::time::FrameworkTime;
use task_macros::task_callback;

use logging::log_file::{LogFileReader, LogFileWriter};
use logging::log_file_json::{JsonLogFileReader, JsonLogFileWriter};

const SOURCE_CHANNEL: &str = "source";
const OUTPUT_CHANNEL: &str = "output";

// ── Graph nodes ────────────────────────────────────────────────────────────

/// Publishes consecutive `u64` values on the source channel, up to `max`.
struct IntegerProducer {
    value: u64,
    max: u64,
    done: Arc<AtomicUsize>,
}

#[task_callback]
impl IntegerProducer {
    fn run(&mut self, mut output: Output<u64>) {
        if self.value >= self.max {
            self.done.store(1, Ordering::Release);
            return;
        }
        *output = self.value;
        self.value += 1;
        output.send();
    }
}

/// Transforms each source value into an output string, recording what it
/// received so the test can assert the reproduced payloads.
struct Doubler {
    received: Arc<Mutex<Vec<(FrameworkTime, u64)>>>,
}

#[task_callback]
impl Doubler {
    fn run(&mut self, mut input: InputSpan<u64>, mut output: Output<String>) {
        if let Some(msg) = input.drain_inputs().next() {
            self.received
                .lock()
                .unwrap()
                .push((msg.header.published_at, msg.message));
            *output = format!("value:{}", msg.message);
            output.send();
        }
    }
}

/// Collects output strings, requesting a stop once `target` have arrived.
struct Collector {
    received: Arc<Mutex<Vec<String>>>,
    stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    target: usize,
}

#[task_callback]
impl Collector {
    fn run(&mut self, mut input: InputSpan<String>) {
        let mut received = self.received.lock().unwrap();
        for msg in input.drain_inputs() {
            received.push(msg.message.clone());
        }
        if received.len() >= self.target
            && let Some(signal) = self.stop_signal.get()
        {
            signal.request_stop();
        }
    }
}

/// A node that only consumes a channel — used to model an external input with
/// no in-graph producer.
struct ExternalConsumer;

#[task_callback]
impl ExternalConsumer {
    fn run(&mut self, mut input: InputSpan<u64>) {
        let _ = input.drain_inputs();
    }
}

fn producer_builder(max: u64, done: Arc<AtomicUsize>) -> CallbackBuilder {
    let done_closure = done.clone();
    CallbackBuilder::new(
        "IntegerProducer".into(),
        Box::new(
            IntegerProducer {
                value: 0,
                max,
                done,
            }
            .build(),
        ),
    )
    .with_publisher_channels(&[SOURCE_CHANNEL])
    .with_execution_duration_callback(|| Duration::from_millis(1))
    .with_next_execution_time_callback(move |now| {
        if done_closure.load(Ordering::Acquire) == 1 {
            None
        } else {
            Some(now + Duration::from_millis(50))
        }
    })
}

fn doubler_builder(received: Arc<Mutex<Vec<(FrameworkTime, u64)>>>) -> CallbackBuilder {
    CallbackBuilder::new("Doubler".into(), Box::new(Doubler { received }.build()))
        .with_subscriber_channels(&[SOURCE_CHANNEL])
        .with_publisher_channels(&[OUTPUT_CHANNEL])
        .with_execution_duration_callback(|| Duration::from_millis(1))
}

fn collector_builder(
    received: Arc<Mutex<Vec<String>>>,
    stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    target: usize,
) -> CallbackBuilder {
    CallbackBuilder::new(
        "Collector".into(),
        Box::new(
            Collector {
                received,
                stop_signal,
                target,
            }
            .build(),
        ),
    )
    .with_subscriber_channels(&[OUTPUT_CHANNEL])
    .with_execution_duration_callback(|| Duration::from_millis(1))
}

fn external_consumer_node(channel: &str) -> CallbackNode {
    CallbackBuilder::new(
        "ExternalConsumer".into(),
        Box::new(ExternalConsumer.build()),
    )
    .with_subscriber_channels(&[channel])
    .with_execution_duration_callback(|| Duration::from_millis(1))
    .build()
    .expect("build external consumer")
}

fn serialize<T: Loggable>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    Loggable::serialize(value, &mut buf).expect("serialize");
    buf
}

// ── Deterministic direct-construction test ─────────────────────────────────

/// Builds a hand-written log for the three-node chain where the `source`
/// channel is deliberately unlogged (only `output` appears in the ordinary
/// log). The producer publishes 0 then 1; the doubler emits `value:0` and
/// `value:1`.
fn write_direct_log(buf: &mut Vec<u8>) {
    let mut desc = ExecutionLogDescriptor::new(&[]);
    desc.index_to_callbacks.insert(
        0,
        task::execution_log::CallbackDescriptor {
            subscriber_index_to_channel_name: HashMap::new(),
            publisher_index_to_channel_name: HashMap::from([(0, SOURCE_CHANNEL.to_string())]),
        },
    );
    desc.index_to_callbacks.insert(
        1,
        task::execution_log::CallbackDescriptor {
            subscriber_index_to_channel_name: HashMap::from([(0, SOURCE_CHANNEL.to_string())]),
            publisher_index_to_channel_name: HashMap::from([(0, OUTPUT_CHANNEL.to_string())]),
        },
    );
    desc.index_to_callbacks.insert(
        2,
        task::execution_log::CallbackDescriptor {
            subscriber_index_to_channel_name: HashMap::from([(0, OUTPUT_CHANNEL.to_string())]),
            publisher_index_to_channel_name: HashMap::new(),
        },
    );

    let mut writer = JsonLogFileWriter::new(buf);
    writer
        .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &serialize(&desc))
        .expect("write descriptor");

    let t0 = FrameworkTime::from_nanoseconds(100);
    let t1 = FrameworkTime::from_nanoseconds(200);
    let t2 = FrameworkTime::from_nanoseconds(300);
    let t3 = FrameworkTime::from_nanoseconds(400);
    let t4 = FrameworkTime::from_nanoseconds(500);
    let t5 = FrameworkTime::from_nanoseconds(600);

    // Ordinary log: ONLY the output channel is logged.
    writer
        .store_message(
            OUTPUT_CHANNEL,
            &MessageHeader::new(t1),
            &serialize(&"value:0".to_string()),
        )
        .expect("output payload");
    writer
        .store_message(
            OUTPUT_CHANNEL,
            &MessageHeader::new(t4),
            &serialize(&"value:1".to_string()),
        )
        .expect("output payload");

    // Execution log: producer publishes 0, doubler transforms + publishes,
    // collector consumes; then the same for value 1.
    let entries = [
        make_entry(
            0,
            t0,
            &[LoggedMessage {
                ordinal: 0,
                direction: Direction::Published,
                header: MessageHeader::new(t0),
            }],
        ),
        make_entry(
            1,
            t1,
            &[
                LoggedMessage {
                    ordinal: 0,
                    direction: Direction::Received,
                    header: MessageHeader::new(t0),
                },
                LoggedMessage {
                    ordinal: 0,
                    direction: Direction::Published,
                    header: MessageHeader::new(t1),
                },
            ],
        ),
        make_entry(
            2,
            t2,
            &[LoggedMessage {
                ordinal: 0,
                direction: Direction::Received,
                header: MessageHeader::new(t1),
            }],
        ),
        make_entry(
            0,
            t3,
            &[LoggedMessage {
                ordinal: 0,
                direction: Direction::Published,
                header: MessageHeader::new(t3),
            }],
        ),
        make_entry(
            1,
            t4,
            &[
                LoggedMessage {
                    ordinal: 0,
                    direction: Direction::Received,
                    header: MessageHeader::new(t3),
                },
                LoggedMessage {
                    ordinal: 0,
                    direction: Direction::Published,
                    header: MessageHeader::new(t4),
                },
            ],
        ),
        make_entry(
            2,
            t5,
            &[LoggedMessage {
                ordinal: 0,
                direction: Direction::Received,
                header: MessageHeader::new(t4),
            }],
        ),
    ];

    for entry in entries {
        let mut msg = ExecutionLogMessage::default();
        msg.entries[0] = entry;
        writer
            .store_message(
                EXECUTION_LOG_CHANNEL,
                &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
                &serialize(&msg),
            )
            .expect("execution log message");
    }
}

fn make_entry(node: u32, time: FrameworkTime, messages: &[LoggedMessage]) -> ExecutionLogEntry {
    let mut entry = ExecutionLogEntry {
        callback_node_index: node,
        execution_time: time,
        execution_duration_ns: 0,
        messages: std::array::from_fn(|_| LoggedMessage::default()),
    };
    for (i, msg) in messages.iter().enumerate() {
        entry.messages[i] = *msg;
    }
    entry
}

fn chain_nodes(
    doubler_received: Arc<Mutex<Vec<(FrameworkTime, u64)>>>,
    collector_received: Arc<Mutex<Vec<String>>>,
    stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
) -> Vec<CallbackNode> {
    vec![
        producer_builder(u64::MAX, Arc::new(AtomicUsize::new(0)))
            .build()
            .expect("build producer"),
        doubler_builder(doubler_received)
            .build()
            .expect("build doubler"),
        collector_builder(collector_received, stop_signal, usize::MAX)
            .build()
            .expect("build collector"),
    ]
}

fn replay_chain(
    nodes: Vec<CallbackNode>,
    buf: &[u8],
) -> exact_replay_executor::ExactReplayExecutor {
    let reader = JsonLogFileReader::from_reader(buf).expect("parse log");

    let mut registry = ChannelRegistry::new();
    registry.register_channel::<u64>(SOURCE_CHANNEL.into());
    registry.register_channel::<String>(OUTPUT_CHANNEL.into());

    let config = exact_replay_executor::ExactReplayConfig::new(
        vec![ThreadPoolConfig::new(1, nodes)],
        registry,
        Box::new(reader),
    );
    exact_replay_executor::ExactReplayExecutor::new(config).expect("build executor")
}

/// The unlogged `source` payloads are reproduced by re-running the producer;
/// the logged `output` payloads still verify the whole computation exactly.
#[test]
#[cfg_attr(miri, ignore)] // the exact replay executor spawns a real thread; slow under Miri
fn replays_unlogged_intermediate_values() {
    let mut buf = Vec::new();
    write_direct_log(&mut buf);

    let doubler_received = Arc::new(Mutex::new(Vec::new()));
    let collector_received = Arc::new(Mutex::new(Vec::new()));
    let stop_signal = Arc::new(OnceLock::new());
    let mut executor = replay_chain(
        chain_nodes(
            doubler_received.clone(),
            collector_received.clone(),
            stop_signal,
        ),
        &buf,
    );

    assert_eq!(executor.execution_count(), 6);

    executor.start();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while executor.is_running() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !executor.is_running(),
        "replay did not finish within deadline"
    );

    let result = executor.stop();
    assert!(result.is_ok(), "replay should succeed: {result:?}");

    let replay_errors = executor.replay_errors();
    assert!(
        replay_errors.is_empty(),
        "expected clean replay, got {replay_errors:?}"
    );

    // The doubler must have received the reproduced source values. The second
    // source value is published at 400ns (the producer's second execution).
    let doubler_received = doubler_received.lock().unwrap();
    assert_eq!(
        doubler_received.as_slice(),
        &[
            (FrameworkTime::from_nanoseconds(100), 0),
            (FrameworkTime::from_nanoseconds(400), 1)
        ],
        "doubler should receive the reproduced source payloads"
    );

    // The collector consumes the logged outputs.
    let collector_received = collector_received.lock().unwrap();
    assert_eq!(
        collector_received.as_slice(),
        &["value:0".to_string(), "value:1".to_string()],
        "collector should receive the logged outputs"
    );

    // The report distinguishes logged from reproduced references.
    let report = executor.replay_report();
    assert!(report.is_exact(), "replay should be exact");
    let ratio = report.exact_reproduction_ratio();
    assert!(
        (ratio - 1.0).abs() < f32::EPSILON,
        "expected ratio ~1.0, got {ratio}"
    );
    assert_eq!(report.logged_count(), 4, "output published + received");
    assert_eq!(report.reproduced_count(), 4, "source published + received");
    let source_stats = &report.channel_stats()[SOURCE_CHANNEL];
    assert_eq!(source_stats.reproduced, 4);
    assert_eq!(source_stats.logged, 0);
}

/// An unlogged channel with no producing node in the graph is unreproducible
/// and fails at construction time.
#[test]
#[cfg_attr(miri, ignore)] // the test binary pulls heavy deps; miri-compiling it is slow
fn unreproducible_source_channel_fails_construction() {
    let mut buf = Vec::new();
    let mut writer = JsonLogFileWriter::new(&mut buf);

    let mut desc = ExecutionLogDescriptor::new(&[]);
    // Node 0 only CONSUMES "external" — nothing produces it.
    desc.index_to_callbacks.insert(
        0,
        task::execution_log::CallbackDescriptor {
            subscriber_index_to_channel_name: HashMap::from([(0, "external".to_string())]),
            publisher_index_to_channel_name: HashMap::new(),
        },
    );
    writer
        .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &serialize(&desc))
        .expect("write descriptor");

    let entry = make_entry(
        0,
        FrameworkTime::from_nanoseconds(100),
        &[LoggedMessage {
            ordinal: 0,
            direction: Direction::Received,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
        }],
    );
    let mut msg = ExecutionLogMessage::default();
    msg.entries[0] = entry;
    writer
        .store_message(
            EXECUTION_LOG_CHANNEL,
            &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
            &serialize(&msg),
        )
        .expect("execution log message");
    finish_writer(writer);

    let reader = JsonLogFileReader::from_reader(buf.as_slice()).expect("parse log");

    let mut registry = ChannelRegistry::new();
    registry.register_channel::<u64>("external".into());

    let config = exact_replay_executor::ExactReplayConfig::new(
        vec![ThreadPoolConfig::new(
            1,
            vec![external_consumer_node("external")],
        )],
        registry,
        Box::new(reader),
    );
    let result = exact_replay_executor::ExactReplayExecutor::new(config);
    let Err(err) = result else {
        panic!("expected construction to fail, got Ok");
    };
    assert!(
        matches!(
            err,
            exact_replay_executor::ReplayError::UnreproducibleMessage { .. }
        ),
        "expected UnreproducibleMessage, got {err:?}"
    );
}

// ── Live end-to-end test ───────────────────────────────────────────────────

/// Runs the chain live with the logging build step configured to log only the
/// output channel, then replays the recorded log and verifies the unlogged
/// source values were reproduced exactly.
#[test]
#[cfg_attr(miri, ignore)] // uses real threads and wall-clock timing
fn replays_a_live_run_with_unlogged_intermediates() {
    const TARGET: usize = 5;

    let log_path = std::env::temp_dir().join(format!(
        "exact_replay_reproduce_{}_{}.ndjson",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    // Live run: producer + doubler + collector + LogTask, execution logging
    // on. Only `String` (output) is registered as loggable, so the `u64`
    // source channel is not logged.
    let doubler_received = Arc::new(Mutex::new(Vec::new()));
    let collector_received = Arc::new(Mutex::new(Vec::new()));
    let stop_signal_cell = Arc::new(OnceLock::new());
    let producer_done = Arc::new(AtomicUsize::new(0));

    let logging_step = Box::new(
        logging::log_build_step::LoggingBuildStep::new(logging::log_task::LogTaskConfiguration {
            output_path: log_path.clone(),
            period: Duration::from_millis(10),
            num_tasks: 1,
        })
        .with_unlogged_channels([SOURCE_CHANNEL]),
    );

    let graph = task::task_graph_builder::TaskGraphBuilder::new()
        .add_pool(1, |p| {
            p.add_callback_builder(producer_builder(TARGET as u64, producer_done.clone()))
                .add_callback_builder(doubler_builder(doubler_received.clone()))
                .add_callback_builder(collector_builder(
                    collector_received.clone(),
                    stop_signal_cell.clone(),
                    usize::MAX,
                ))
        })
        .add_build_step(logging_step)
        .with_log_executions(true)
        .build()
        .expect("build live graph");

    let mut exec = live_executor::LiveExecutor::new_multi_pool_with_execution_log(
        graph.pools,
        graph.execution_log_publishers,
        Duration::from_millis(50),
    );
    stop_signal_cell.set(exec.stop_signal()).ok();
    exec.start_threads();

    let deadline = std::time::Instant::now() + Duration::from_secs(35);
    while exec.is_running()
        && collector_received.lock().unwrap().len() < TARGET
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        collector_received.lock().unwrap().len() >= TARGET,
        "live run did not reach {TARGET} outputs in time"
    );
    // Wait for the LogTask to drain every output into the file before stopping
    // (the producer stops at TARGET, so the log should settle on TARGET output
    // entries). Polling beats a fixed sleep: under a loaded test runner the
    // LogTask can lag arbitrarily far behind.
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(35);
    while count_logged_outputs(&log_path) < TARGET && std::time::Instant::now() < drain_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        count_logged_outputs(&log_path) >= TARGET,
        "LogTask did not drain {TARGET} outputs into the log in time"
    );
    // Let the executor flush the final execution-log batch before stopping.
    std::thread::sleep(Duration::from_millis(100));
    exec.stop_threads().expect("stop live threads");

    let live_outputs = collector_received.lock().unwrap().clone();
    assert!(
        live_outputs.len() >= TARGET,
        "live run should produce at least {TARGET} outputs, got {}",
        live_outputs.len()
    );

    // The descriptor in the recorded log must annotate only the output channel.
    {
        let file = std::fs::File::open(&log_path).expect("open recorded log");
        let reader =
            JsonLogFileReader::from_reader(std::io::BufReader::new(file)).expect("parse log");
        let desc = reader
            .artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT)
            .expect("descriptor artifact");
        let desc: ExecutionLogDescriptor = serde_json::from_slice(desc).expect("parse descriptor");
        assert!(
            desc.logged_channels.contains(OUTPUT_CHANNEL),
            "descriptor should annotate the output channel as logged"
        );
        assert!(
            !desc.logged_channels.contains(SOURCE_CHANNEL),
            "descriptor must not annotate the source channel as logged"
        );
    }

    // Replay the recorded log through a fresh chain.
    let file = std::fs::File::open(&log_path).expect("open recorded log");
    let reader = JsonLogFileReader::from_reader(std::io::BufReader::new(file)).expect("parse log");

    let replay_doubler = Arc::new(Mutex::new(Vec::new()));
    let replay_collector = Arc::new(Mutex::new(Vec::new()));
    let replay_stop = Arc::new(OnceLock::new());
    let nodes = chain_nodes(
        replay_doubler.clone(),
        replay_collector.clone(),
        replay_stop,
    );

    let mut replay_registry = ChannelRegistry::new();
    replay_registry.register_channel::<u64>(SOURCE_CHANNEL.into());
    replay_registry.register_channel::<String>(OUTPUT_CHANNEL.into());

    let config = exact_replay_executor::ExactReplayConfig::new(
        vec![ThreadPoolConfig::new(1, nodes)],
        replay_registry,
        Box::new(reader),
    );
    let mut executor =
        exact_replay_executor::ExactReplayExecutor::new(config).expect("build replay executor");

    assert!(
        executor.execution_count() > 0,
        "recorded log has executions"
    );

    executor.start();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while executor.is_running() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !executor.is_running(),
        "replay did not finish within deadline"
    );

    let stop_result = executor.stop();
    assert!(stop_result.is_ok(), "replay diverged: {stop_result:?}");

    let replay_errors = executor.replay_errors();
    assert!(
        replay_errors.is_empty(),
        "expected clean replay, got {replay_errors:?}"
    );

    // The replayed collector must see exactly the outputs the live run saw.
    let replay_collector = replay_collector.lock().unwrap();
    assert_eq!(
        replay_collector.as_slice(),
        live_outputs.as_slice(),
        "replay should reproduce every logged output exactly"
    );

    // The doubler must have been fed reproduced source values matching the
    // live sequence 0..TARGET.
    let replay_doubler = replay_doubler.lock().unwrap();
    assert_eq!(
        replay_doubler.len(),
        live_outputs.len(),
        "doubler should receive one reproduced source value per output"
    );
    let values: Vec<u64> = replay_doubler.iter().map(|(_, v)| *v).collect();
    assert_eq!(
        values,
        (0..live_outputs.len() as u64).collect::<Vec<_>>(),
        "reproduced source values should be the deterministic sequence"
    );

    let report = executor.replay_report();
    assert!(report.is_exact(), "live replay should be exact");
    let ratio = report.exact_reproduction_ratio();
    assert!(
        (ratio - 1.0).abs() < f32::EPSILON,
        "expected ratio ~1.0, got {ratio}"
    );
    assert!(
        report.reproduced_count() >= live_outputs.len() * 2,
        "expected at least one reproduced reference per source message, got {}",
        report.reproduced_count()
    );
}

// `JsonLogFileWriter` borrows the backing buffer but has no Drop
// implementation. Consume it explicitly so that the borrow ends without
// triggering clippy::drop_non_drop.
fn finish_writer<W: std::io::Write>(_: JsonLogFileWriter<W>) {}

/// Count ordinary-log entries for `OUTPUT_CHANNEL` in an NDJSON log file that
/// is still being written. Used to wait for the LogTask to drain before
/// stopping the live executor.
fn count_logged_outputs(path: &std::path::Path) -> usize {
    let Ok(data) = std::fs::read_to_string(path) else {
        return 0;
    };
    data.lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| {
                v.get("channel_name")
                    .and_then(|ch| ch.as_str())
                    .is_some_and(|ch| ch == OUTPUT_CHANNEL)
            })
        })
        .count()
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);
