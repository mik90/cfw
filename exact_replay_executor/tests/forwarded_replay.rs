//! End-to-end tests for replaying graphs that use forwarded messages.
//!
//! A forwarding graph has three roles:
//! - a producer publishes a payload on a *source* channel,
//! - a forwarder wraps that payload in a `ForwardedMessage<T, F>` (extra data
//!   `T` plus a reference to the source message) on a *forwarded* channel,
//! - a consumer subscribes to the forwarded channel.
//!
//! The forwarded message's serialized body only carries `T` and the forwarded
//! header — the payload `F` lives on the source channel. Replay reconstructs
//! the payload from the source channel's logged messages via a
//! `ForwardedMessageContext`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use task::callback::{Callback, CallbackNode, PortMut, Run};
use task::callback_builder::CallbackBuilder;
use task::channel_registry::ChannelRegistry;
use task::context::Context;
use task::execution_log::{
    Direction, EXECUTION_LOG_CHANNEL, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, ExecutionLogDescriptor,
    ExecutionLogEntry, ExecutionLogLevel, ExecutionLogMessage, LoggedMessage,
};
use task::executor::{Executor, ExecutorStopSignal, ThreadPoolConfig};
use task::forwarded_message::ForwardedMessage;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::input::{ForwardableOptionalInput, InputSpan};
use task::loggable::Loggable;
use task::message::{Message, MessageHeader};
use task::output::{ForwardingOutput, Output};
use task::publisher::{ForwardingPublisher, Publisher, PublisherConfig};
use task::subscriber::{ForwardableSubscriber, Subscriber, SubscriberConfig};
use task::time::FrameworkTime;

use logging::log_file::LogFileWriter;
use logging::log_file_json::{JsonLogFileReader, JsonLogFileWriter};

const SOURCE_CHANNEL: &str = "source";
const FORWARDED_CHANNEL: &str = "forwarded";

// ── Graph nodes ────────────────────────────────────────────────────────────

/// Publishes consecutive `u32` values on the source channel, up to `max`.
struct IntegerProducer {
    publisher: Publisher<u32>,
    value: u32,
    max: u32,
    done: Arc<AtomicUsize>,
}

impl Callback for IntegerProducer {
    fn run(&mut self, _ctx: &Context) -> Run {
        if self.value >= self.max {
            self.done.store(1, Ordering::Release);
            return Run::new(0);
        }
        let mut output = Output::new_default(&mut self.publisher);
        *output = self.value;
        self.value += 1;
        output.send();
        Run::new(1)
    }
    fn register_channels(&self, registry: &mut ChannelRegistry) {
        // Hand-rolled callback: register the concrete port type explicitly.
        task::channel_registry::Probe::<u32>::new().try_register(registry);
        task::channel_registry::Probe::<u32>::new()
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

/// Forwards source messages, tagging each with a `bool` payload.
struct Forwarder {
    subscriber: ForwardableSubscriber<u32>,
    publisher: ForwardingPublisher<bool, u32>,
}

impl Callback for Forwarder {
    fn run(&mut self, _ctx: &Context) -> Run {
        let input = ForwardableOptionalInput::new(&self.subscriber);
        if let Some(mut fwd) = input.forward(&mut ForwardingOutput::new(&mut self.publisher)) {
            *fwd = true;
            fwd.send();
        }
        Run::new(1)
    }
    fn register_channels(&self, registry: &mut ChannelRegistry) {
        // Hand-rolled callback: register the concrete port types explicitly.
        // The forwarded channel only gets its serializer here — the full
        // channel mapping (deserializer + publisher factory) is registered by
        // the replay executor via `register_forwarded_channel`.
        use task::channel_registry::MaybeRegister as _;
        task::channel_registry::Probe::<u32>::new().try_register(registry);
        task::channel_registry::Probe::<u32>::new()
            .try_register_channel(registry, self.subscriber.config().channel_name.clone());
        task::channel_registry::Probe::<ForwardedMessage<bool, u32>>::new().try_register(registry);
        task::channel_registry::Probe::<ForwardedMessage<bool, u32>>::new()
            .try_register_channel(registry, self.publisher.config().channel_name.clone());
    }
    fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
        f(&self.subscriber);
    }
    fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
        f(&self.publisher);
    }
    fn for_each_subscriber_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericSubscriber)) {
        f(&mut self.subscriber);
    }
    fn for_each_publisher_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericPublisher)) {
        f(&mut self.publisher);
    }
    fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
        f(PortMut::Subscriber(&mut self.subscriber));
        f(PortMut::Publisher(&mut self.publisher));
    }
}

/// Consumes forwarded messages, recording the decoded `(bool, u32)` pairs.
struct Consumer {
    subscriber: Subscriber<ForwardedMessage<bool, u32>>,
    received: Arc<Mutex<Vec<(bool, u32)>>>,
    stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    target: usize,
}

impl Callback for Consumer {
    fn run(&mut self, _ctx: &Context) -> Run {
        let mut input =
            InputSpan::<ForwardedMessage<bool, u32>>::new_downcasted(&mut self.subscriber);
        let mut received = self.received.lock().unwrap();
        for msg in input.drain_inputs() {
            received.push((
                *msg.message.message(),
                msg.message.forwarded_message().message,
            ));
        }
        if received.len() >= self.target
            && let Some(signal) = self.stop_signal.get()
        {
            signal.request_stop();
        }
        Run::new(1)
    }
    fn register_channels(&self, registry: &mut ChannelRegistry) {
        // Hand-rolled callback: register the concrete port type explicitly.
        use task::channel_registry::MaybeRegister as _;
        task::channel_registry::Probe::<ForwardedMessage<bool, u32>>::new().try_register(registry);
        task::channel_registry::Probe::<ForwardedMessage<bool, u32>>::new()
            .try_register_channel(registry, self.subscriber.config().channel_name.clone());
    }
    fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
        f(&self.subscriber);
    }
    fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
    fn for_each_subscriber_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericSubscriber)) {
        f(&mut self.subscriber);
    }
    fn for_each_publisher_mut<'a>(&'a mut self, _f: &mut dyn FnMut(&'a mut dyn GenericPublisher)) {}
    fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
        f(PortMut::Subscriber(&mut self.subscriber));
    }
}

fn producer_builder(max: u32, done: Arc<AtomicUsize>) -> CallbackBuilder {
    let done_closure = done.clone();
    CallbackBuilder::new(
        "IntegerProducer".into(),
        Box::new(IntegerProducer {
            publisher: Publisher::new(PublisherConfig {
                capacity: 1,
                channel_name: String::new(),
            }),
            value: 0,
            max,
            done,
        }),
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

fn forwarder_builder() -> CallbackBuilder {
    CallbackBuilder::new(
        "Forwarder".into(),
        Box::new(Forwarder {
            subscriber: ForwardableSubscriber::new(SubscriberConfig {
                is_optional: true,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: String::new(),
            }),
            publisher: ForwardingPublisher::new(
                PublisherConfig {
                    capacity: 1,
                    channel_name: String::new(),
                },
                vec![FORWARDED_CHANNEL.into()],
            ),
        }),
    )
    .with_subscriber_channels(&[SOURCE_CHANNEL])
    .with_publisher_channels(&[FORWARDED_CHANNEL])
    .with_execution_duration_callback(|| Duration::from_millis(1))
}

fn consumer_builder(
    received: Arc<Mutex<Vec<(bool, u32)>>>,
    stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    target: usize,
) -> CallbackBuilder {
    CallbackBuilder::new(
        "Consumer".into(),
        Box::new(Consumer {
            subscriber: Subscriber::<ForwardedMessage<bool, u32>>::new(SubscriberConfig {
                is_optional: false,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: String::new(),
            }),
            received,
            stop_signal,
            target,
        }),
    )
    .with_subscriber_channels(&[FORWARDED_CHANNEL])
    .with_execution_duration_callback(|| Duration::from_millis(1))
}

// ── Deterministic direct-construction test ─────────────────────────────────

fn serialize<T: Loggable>(value: &T) -> Vec<u8> {
    let mut buf = Vec::new();
    Loggable::serialize(value, &mut buf).expect("serialize");
    buf
}

fn make_entry(node: u32, time_ns: i64, messages: &[LoggedMessage]) -> ExecutionLogEntry {
    let mut entry = ExecutionLogEntry {
        callback_node_index: node,
        execution_time: FrameworkTime::from_nanoseconds(time_ns),
        execution_duration_ns: 0,
        log_whole: true,
        messages: std::array::from_fn(|_| LoggedMessage::default()),
    };
    for (i, msg) in messages.iter().enumerate() {
        entry.messages[i] = *msg;
    }
    entry
}

fn write_direct_log(buf: &mut Vec<u8>) {
    // Graph: producer(source) -> forwarder(source, forwarded) -> consumer(forwarded)
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
            publisher_index_to_channel_name: HashMap::from([(0, FORWARDED_CHANNEL.to_string())]),
        },
    );
    desc.index_to_callbacks.insert(
        2,
        task::execution_log::CallbackDescriptor {
            subscriber_index_to_channel_name: HashMap::from([(0, FORWARDED_CHANNEL.to_string())]),
            publisher_index_to_channel_name: HashMap::new(),
        },
    );

    let mut writer = JsonLogFileWriter::new(buf);
    writer
        .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &serialize(&desc))
        .expect("write descriptor");

    let t_source = FrameworkTime::from_nanoseconds(100);
    let t_forwarded = FrameworkTime::from_nanoseconds(200);

    // Ordinary log: the source payload and the forwarded message body.
    writer
        .store_message(
            SOURCE_CHANNEL,
            &MessageHeader::new(t_source),
            &serialize(&0u32),
        )
        .expect("source payload");
    let forwarded_body = serialize(&ForwardedMessage::new_boxed_forward(
        true,
        Box::new(Message {
            header: MessageHeader::new(t_source),
            message: 0u32,
        }),
    ));
    writer
        .store_message(
            FORWARDED_CHANNEL,
            &MessageHeader::new(t_forwarded),
            &forwarded_body,
        )
        .expect("forwarded payload");

    // Execution log: producer publishes, forwarder receives+forwards,
    // consumer receives.
    let entries = [
        make_entry(
            0,
            100,
            &[LoggedMessage {
                ordinal: 0,
                direction: Direction::Published,
                header: MessageHeader::new(t_source),
            }],
        ),
        make_entry(
            1,
            200,
            &[
                LoggedMessage {
                    ordinal: 0,
                    direction: Direction::Received,
                    header: MessageHeader::new(t_source),
                },
                LoggedMessage {
                    ordinal: 0,
                    direction: Direction::Published,
                    header: MessageHeader::new(t_forwarded),
                },
            ],
        ),
        make_entry(
            2,
            300,
            &[LoggedMessage {
                ordinal: 0,
                direction: Direction::Received,
                header: MessageHeader::new(t_forwarded),
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

fn replay_graph(nodes: Vec<CallbackNode>) -> (exact_replay_executor::ExactReplayExecutor, Vec<u8>) {
    let mut buf = Vec::new();
    write_direct_log(&mut buf);
    let reader = JsonLogFileReader::from_reader(buf.as_slice()).expect("parse log");

    let mut registry = ChannelRegistry::new();
    registry.register_channel::<u32>(SOURCE_CHANNEL.into());
    registry
        .register_forwarded_channel::<bool, u32>(FORWARDED_CHANNEL.into(), SOURCE_CHANNEL.into());

    let config = exact_replay_executor::ExactReplayConfig::new(
        vec![ThreadPoolConfig::new(1, nodes)],
        registry,
        Box::new(reader),
    );
    let executor = exact_replay_executor::ExactReplayExecutor::new(config).expect("build executor");
    (executor, buf)
}

#[test]
#[cfg_attr(miri, ignore)] // the exact replay executor spawns a real thread; slow under Miri
fn replays_forwarded_messages_end_to_end() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let stop_signal = Arc::new(OnceLock::new());
    let nodes = vec![
        producer_builder(1, Arc::new(AtomicUsize::new(0)))
            .build()
            .expect("build producer"),
        forwarder_builder().build().expect("build forwarder"),
        consumer_builder(received.clone(), stop_signal, usize::MAX)
            .build()
            .expect("build consumer"),
    ];

    let (mut executor, _buf) = replay_graph(nodes);
    assert_eq!(executor.execution_count(), 3);

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

    // The consumer must have reconstructed the forwarded payload from the
    // source channel's logged message (the forwarded body only carries the
    // header + extra data).
    let received = received.lock().unwrap();
    assert_eq!(
        received.as_slice(),
        &[(true, 0u32)],
        "consumer should have received the reconstructed forwarded payload"
    );
}

// ── Live end-to-end test ───────────────────────────────────────────────────

#[test]
#[cfg_attr(miri, ignore)] // uses real threads and wall-clock timing
fn replays_a_live_forwarding_run() {
    const TARGET: usize = 5;

    let log_path = std::env::temp_dir().join(format!(
        "exact_replay_fwd_{}_{}.ndjson",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    // Live run: forwarder + consumer + LogTask, execution logging on. The
    // producer stops after TARGET values; the test stops the executor once the
    // consumer has seen them all and the LogTask has had time to drain.
    let received = Arc::new(Mutex::new(Vec::new()));
    let stop_signal_cell = Arc::new(OnceLock::new());
    let producer_done = Arc::new(AtomicUsize::new(0));

    let logging_step = Box::new(logging::log_build_step::LoggingBuildStep::new(
        logging::log_task::LogTaskConfiguration {
            output_path: log_path.clone(),
            period: Duration::from_millis(10),
            num_tasks: 1,
        },
    ));

    let graph = task::task_graph_builder::TaskGraphBuilder::new()
        .add_pool(1, |p| {
            p.add_callback_builder(producer_builder(TARGET as u32, producer_done.clone()))
                .add_callback_builder(forwarder_builder())
                .add_callback_builder(consumer_builder(
                    received.clone(),
                    stop_signal_cell.clone(),
                    usize::MAX,
                ))
        })
        .add_build_step(logging_step)
        .with_execution_log_level(ExecutionLogLevel::Whole)
        .build()
        .expect("build live graph");

    let mut exec = live_executor::LiveExecutor::new_multi_pool_with_execution_log(
        graph.pools,
        graph.execution_log_publishers,
        Duration::from_millis(50),
    );
    stop_signal_cell.set(exec.stop_signal()).ok();
    exec.start_threads();

    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    while exec.is_running()
        && received.lock().unwrap().len() < TARGET
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        received.lock().unwrap().len() >= TARGET,
        "live forwarding run did not reach {TARGET} forwarded messages in time"
    );
    // Give the LogTask time to drain the final messages into the file before
    // the executor stops.
    std::thread::sleep(Duration::from_millis(300));
    exec.stop_threads().expect("stop live threads");

    let live_received = received.lock().unwrap().clone();
    assert!(
        live_received.len() >= TARGET,
        "live run should forward at least {TARGET} messages, got {}",
        live_received.len()
    );

    // Replay the recorded log through a fresh forwarding graph.
    let file = std::fs::File::open(&log_path).expect("open recorded log");
    let reader = JsonLogFileReader::from_reader(std::io::BufReader::new(file)).expect("parse log");

    let replay_received = Arc::new(Mutex::new(Vec::new()));
    let replay_stop = Arc::new(OnceLock::new());
    let nodes = vec![
        producer_builder(u32::MAX, Arc::new(AtomicUsize::new(0)))
            .build()
            .expect("build producer"),
        forwarder_builder().build().expect("build forwarder"),
        consumer_builder(replay_received.clone(), replay_stop, usize::MAX)
            .build()
            .expect("build consumer"),
    ];

    let mut replay_registry = ChannelRegistry::new();
    replay_registry.register_channel::<u32>(SOURCE_CHANNEL.into());
    replay_registry
        .register_forwarded_channel::<bool, u32>(FORWARDED_CHANNEL.into(), SOURCE_CHANNEL.into());

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

    // The replay must reconstruct every forwarded message the live run saw,
    // with the same extra payload and the same forwarded (source) payload.
    let replay_received = replay_received.lock().unwrap();
    assert_eq!(
        replay_received.as_slice(),
        live_received.as_slice(),
        "replay should reconstruct every forwarded payload exactly"
    );
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);
