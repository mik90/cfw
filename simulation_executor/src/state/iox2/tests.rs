use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iceoryx2::prelude::EventId;
use task::callback::{Callback, PubOrSub, PubOrSubMut};
use task::context::Context;
use task::executor::ExecutorParams;
use task::iox2::{
    Iox2Event, Iox2EventSubscriber, Iox2GraphConfig, Iox2Notifier, Iox2NotifyOutput, Iox2OpenCtx,
    Iox2OptionalInput, Iox2Subscriber,
};
use task::output::Output;
use task::publisher::{Publisher, PublisherConfig};
use task::subscriber::{Subscriber, SubscriberConfig};
use task::task_graph_builder::TaskGraphBuilder;
use task::time::FrameworkTime;

use crate::SimulationConfig;
use crate::executor::SimulationExecutor;

use super::SimulationState;

static NEXT_CHANNEL: AtomicUsize = AtomicUsize::new(0);
type ObservedEvents = Arc<Mutex<Vec<Vec<(EventId, u64)>>>>;
type ObservedSyntheticInputs = Arc<Mutex<Vec<(Option<(u64, FrameworkTime)>, Vec<(EventId, u64)>)>>>;

fn channel(label: &str) -> String {
    format!(
        "sim_{label}_{}_{}",
        std::process::id(),
        NEXT_CHANNEL.fetch_add(1, Ordering::Relaxed)
    )
}

struct GatePublisher(Publisher<u64>);

impl Callback for GatePublisher {
    fn run(&mut self, _ctx: &Context) {
        let mut output = Output::new_default(&mut self.0);
        *output = 42;
        output.send();
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Publisher(&self.0));
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Publisher(&mut self.0));
    }
}

struct EventConsumer {
    event: Iox2EventSubscriber,
    required: Option<Subscriber<u64>>,
    observed: ObservedEvents,
}

impl Callback for EventConsumer {
    fn run(&mut self, _ctx: &Context) {
        let event = Iox2Event::new(&self.event);
        self.observed
            .lock()
            .unwrap()
            .push(event.records().collect());
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Subscriber(&self.event));
        if let Some(required) = &self.required {
            f(PubOrSub::Subscriber(required));
        }
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Subscriber(&mut self.event));
        if let Some(required) = &mut self.required {
            f(PubOrSubMut::Subscriber(required));
        }
    }
}

struct EventProducer {
    notifier: Iox2Notifier,
}

impl Callback for EventProducer {
    fn run(&mut self, _ctx: &Context) {
        Iox2NotifyOutput::new(&mut self.notifier).send();
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Publisher(&self.notifier));
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Publisher(&mut self.notifier));
    }
}

struct BlockingPeriodic {
    entered: Option<std::sync::mpsc::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Callback for BlockingPeriodic {
    fn run(&mut self, _ctx: &Context) {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
            let _ = self.release.recv();
        }
    }

    fn for_each_pub_or_sub<'a>(&'a self, _f: &mut dyn FnMut(PubOrSub<'a>)) {}

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, _f: &mut dyn FnMut(PubOrSubMut<'a>)) {}
}

struct ObservedEventConsumer {
    event: Iox2EventSubscriber,
    observed: std::sync::mpsc::Sender<Vec<(EventId, u64)>>,
}

impl Callback for ObservedEventConsumer {
    fn run(&mut self, _ctx: &Context) {
        let event = Iox2Event::new(&self.event);
        let _ = self.observed.send(event.records().collect());
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Subscriber(&self.event));
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Subscriber(&mut self.event));
    }
}

fn node(name: &str, callback: Box<dyn Callback>) -> task::callback::CallbackNode {
    let mut node = task::callback::CallbackNode::new_named(callback, name.into());
    node.set_execution_duration_callback(Box::new(|| Duration::ZERO));
    node
}

struct SyntheticInputConsumer {
    data: Iox2Subscriber<u64>,
    event: Iox2EventSubscriber,
    observed: ObservedSyntheticInputs,
}

impl Callback for SyntheticInputConsumer {
    fn run(&mut self, _ctx: &Context) {
        let data = Iox2OptionalInput::new(&self.data);
        let observed_data = data
            .value()
            .copied()
            .zip(data.header().map(|header| header.published_at));
        drop(data);
        let event = Iox2Event::new(&self.event);
        self.observed
            .lock()
            .unwrap()
            .push((observed_data, event.records().collect()));
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Subscriber(&self.data));
        f(PubOrSub::Subscriber(&self.event));
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Subscriber(&mut self.data));
        f(PubOrSubMut::Subscriber(&mut self.event));
    }
}

fn synthetic_consumer(
    name: &str,
    data_channel: &str,
    event_channel: &str,
    observed: ObservedSyntheticInputs,
) -> task::callback::CallbackNode {
    node(
        name,
        Box::new(SyntheticInputConsumer {
            data: Iox2Subscriber::new(SubscriberConfig {
                is_optional: true,
                capacity: 4,
                is_trigger: false,
                keep_across_runs: true,
                channel_name: data_channel.into(),
            }),
            event: Iox2EventSubscriber::new(SubscriberConfig {
                is_optional: true,
                capacity: 4,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: event_channel.into(),
            }),
            observed,
        }),
    )
}

fn params_from_graph(mut graph: task::task_graph_builder::BuiltTaskGraph) -> ExecutorParams {
    ExecutorParams::new(std::mem::take(&mut graph.pools))
        .with_iox2_context(graph.iox2_context.take())
}

/// The scheduler sees listener activations only at step boundaries and retains them behind required-input gates.
#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn event_polling_respects_step_boundaries_and_required_inputs() {
    let event_channel = channel("gate_event");
    let gate_channel = channel("gate_data");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let consumer = node(
        "event_consumer",
        Box::new(EventConsumer {
            event: Iox2EventSubscriber::new(SubscriberConfig {
                is_optional: true,
                capacity: 4,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: event_channel.clone(),
            }),
            required: Some(Subscriber::new(SubscriberConfig {
                is_optional: false,
                capacity: 1,
                is_trigger: false,
                keep_across_runs: true,
                channel_name: gate_channel.clone(),
            })),
            observed: observed.clone(),
        }),
    );
    let gate_publisher = node(
        "gate_publisher",
        Box::new(GatePublisher(Publisher::new(PublisherConfig {
            capacity: 1,
            channel_name: gate_channel,
        }))),
    );
    let mut graph = TaskGraphBuilder::new()
        .with_iox2_config(Iox2GraphConfig::default())
        .add_pool(2, |pool| {
            pool.add_callback(consumer).add_callback(gate_publisher)
        })
        .build()
        .expect("iox2 simulation graph builds");
    let notifier = graph
        .iox2_context
        .as_mut()
        .unwrap()
        .event_service(&event_channel)
        .unwrap()
        .notifier_builder()
        .default_event_id(EventId::new(0))
        .create()
        .unwrap();
    let params = ExecutorParams::new(std::mem::take(&mut graph.pools))
        .with_iox2_context(graph.iox2_context.take());
    let mut state = SimulationState::try_new_with(SimulationConfig {
        start_time: FrameworkTime::from_nanoseconds(0),
        executor_params: params,
        node_executor_thread_count: 2,
    })
    .unwrap();
    state.start();
    notifier.notify().unwrap();
    notifier.notify().unwrap();
    notifier.notify().unwrap();
    state
        .schedule_iox2_event(
            FrameworkTime::from_nanoseconds(0),
            &event_channel,
            EventId::new(1),
            4,
        )
        .unwrap();

    assert!(state.step().unwrap().is_empty());
    assert!(observed.lock().unwrap().is_empty());

    let names = task::string_interner::ChannelNameInterner::default();
    let callbacks = task::string_interner::CallbackNameInterner::default();
    let ctx = Context::new(state.time, &names, &callbacks);
    state.nodes[1].access(|node| {
        node.run(&ctx);
        node.flush_publishers(ctx.now, &mut task::scheduling::NoopReadyNodeSink);
    });

    assert_eq!(state.step().unwrap(), vec![0]);
    assert_eq!(
        *observed.lock().unwrap(),
        vec![vec![(EventId::new(1), 4), (EventId::new(0), 3)]]
    );
}

/// Notifications emitted by callback output flushes are polled at the following step boundary.
#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn callback_notification_is_visible_on_next_step() {
    let event_channel = channel("next_step");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut producer = node(
        "event_producer",
        Box::new(EventProducer {
            notifier: Iox2Notifier::new(PublisherConfig {
                capacity: 1,
                channel_name: event_channel.clone(),
            }),
        }),
    );
    producer.set_execution_time_callback(Box::new(|now| Some(now + Duration::from_secs(1))));
    let consumer = node(
        "event_consumer",
        Box::new(EventConsumer {
            event: Iox2EventSubscriber::new(SubscriberConfig {
                is_optional: true,
                capacity: 4,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: event_channel,
            }),
            required: None,
            observed: observed.clone(),
        }),
    );
    let mut graph = TaskGraphBuilder::new()
        .with_iox2_config(Iox2GraphConfig::default())
        .add_pool(2, |pool| pool.add_callback(producer).add_callback(consumer))
        .build()
        .expect("iox2 simulation graph builds");
    let params = ExecutorParams::new(std::mem::take(&mut graph.pools))
        .with_iox2_context(graph.iox2_context.take());
    let mut state = SimulationState::try_new_with(SimulationConfig {
        start_time: FrameworkTime::from_nanoseconds(0),
        executor_params: params,
        node_executor_thread_count: 2,
    })
    .unwrap();
    state.start();

    assert_eq!(state.step().unwrap(), vec![0]);
    assert!(observed.lock().unwrap().is_empty());
    assert!(state.step().unwrap().contains(&1));
    assert_eq!(*observed.lock().unwrap(), vec![vec![(EventId::new(0), 1)]]);
}

/// Scheduled data uses real IPC samples while events fan out in stable order at step boundaries.
#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn scheduled_data_and_events_are_deterministic() {
    let data_channel = channel("synthetic_data");
    let event_channel = channel("synthetic_event");
    let first = Arc::new(Mutex::new(Vec::new()));
    let second = Arc::new(Mutex::new(Vec::new()));
    let graph = TaskGraphBuilder::new()
        .with_iox2_config(Iox2GraphConfig::default())
        .add_pool(2, |pool| {
            pool.add_callback(synthetic_consumer(
                "first_consumer",
                &data_channel,
                &event_channel,
                first.clone(),
            ))
            .add_callback(synthetic_consumer(
                "second_consumer",
                &data_channel,
                &event_channel,
                second.clone(),
            ))
        })
        .build()
        .expect("synthetic iox2 graph builds");
    let mut state = SimulationState::try_new_with(SimulationConfig {
        start_time: FrameworkTime::from_nanoseconds(0),
        executor_params: params_from_graph(graph),
        node_executor_thread_count: 2,
    })
    .unwrap();
    let sample_time = FrameworkTime::from_nanoseconds(21);
    assert!(matches!(
        state.schedule_iox2_data(sample_time, &data_channel, 1u32),
        Err(crate::state::Iox2InputError::PayloadTypeMismatch(_))
    ));
    assert!(matches!(
        state.schedule_iox2_data(sample_time, "not_a_data_channel", 1u64),
        Err(crate::state::Iox2InputError::UnknownDataChannel(_))
    ));
    state
        .schedule_iox2_data(sample_time, &data_channel, 99u64)
        .unwrap();
    state.start();

    assert!(state.step().unwrap().is_empty());
    assert_eq!(state.simulation_time(), sample_time);
    assert!(state.step().unwrap().is_empty());
    assert!(first.lock().unwrap().is_empty());
    assert!(second.lock().unwrap().is_empty());

    state
        .schedule_iox2_event(sample_time, &event_channel, EventId::new(6), 9_000_000_000)
        .unwrap();
    state
        .schedule_iox2_event(sample_time, &event_channel, EventId::new(3), 2)
        .unwrap();

    assert_eq!(state.step().unwrap(), vec![0, 1]);
    let expected = vec![(
        Some((99, sample_time)),
        vec![(EventId::new(6), 9_000_000_000), (EventId::new(3), 2)],
    )];
    assert_eq!(*first.lock().unwrap(), expected);
    assert_eq!(*second.lock().unwrap(), expected);

    let late_time = FrameworkTime::from_nanoseconds(1);
    state
        .schedule_iox2_data(late_time, &data_channel, 100u64)
        .unwrap();
    state
        .schedule_iox2_event(late_time, &event_channel, EventId::new(1), 4)
        .unwrap();
    assert_eq!(state.step().unwrap(), vec![0, 1]);
    assert_eq!(state.simulation_time(), sample_time);
    assert_eq!(first.lock().unwrap()[1].0, Some((100, late_time)));
    assert_eq!(first.lock().unwrap()[1].1, vec![(EventId::new(1), 4)]);
}

/// A future event input advances an otherwise idle simulation clock.
#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn future_event_advances_simulation_time() {
    let event_channel = channel("future_event");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let graph = TaskGraphBuilder::new()
        .with_iox2_config(Iox2GraphConfig::default())
        .add_pool(1, |pool| {
            pool.add_callback(node(
                "event_consumer",
                Box::new(EventConsumer {
                    event: Iox2EventSubscriber::new(SubscriberConfig {
                        is_optional: true,
                        capacity: 2,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: event_channel.clone(),
                    }),
                    required: None,
                    observed: observed.clone(),
                }),
            ))
        })
        .build()
        .unwrap();
    let mut state = SimulationState::try_new_with(SimulationConfig {
        start_time: FrameworkTime::from_nanoseconds(0),
        executor_params: params_from_graph(graph),
        node_executor_thread_count: 1,
    })
    .unwrap();
    let scheduled_at = FrameworkTime::from_nanoseconds(44);
    state
        .schedule_iox2_event(scheduled_at, &event_channel, EventId::new(5), 81)
        .unwrap();
    state.start();

    assert!(state.step().unwrap().is_empty());
    assert_eq!(state.simulation_time(), scheduled_at);
    assert_eq!(state.step().unwrap(), vec![0]);
    assert_eq!(*observed.lock().unwrap(), vec![vec![(EventId::new(5), 81)]]);
}

/// Executor-side scheduling serializes behind an active step and becomes visible on a later boundary.
#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn simulation_executor_scheduling_waits_for_active_step() {
    use task::executor::Executor;

    let event_channel = channel("executor_schedule");
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (observed_tx, observed_rx) = std::sync::mpsc::channel();
    let mut blocker = node(
        "blocking_periodic",
        Box::new(BlockingPeriodic {
            entered: Some(entered_tx),
            release: release_rx,
        }),
    );
    blocker.set_execution_time_callback(Box::new(|now| Some(now + Duration::from_secs(1))));
    let consumer = node(
        "event_consumer",
        Box::new(ObservedEventConsumer {
            event: Iox2EventSubscriber::new(SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: event_channel.clone(),
            }),
            observed: observed_tx,
        }),
    );
    let graph = TaskGraphBuilder::new()
        .with_iox2_config(Iox2GraphConfig::default())
        .add_pool(2, |pool| pool.add_callback(blocker).add_callback(consumer))
        .build()
        .unwrap();
    let mut executor = SimulationExecutor::try_new_with(SimulationConfig {
        start_time: FrameworkTime::from_nanoseconds(0),
        executor_params: params_from_graph(graph),
        node_executor_thread_count: 2,
    })
    .unwrap();
    executor.start();
    entered_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("background callback did not enter");

    let executor = Arc::new(executor);
    let scheduling_executor = executor.clone();
    let (scheduled_tx, scheduled_rx) = std::sync::mpsc::channel();
    let scheduling_thread = std::thread::spawn(move || {
        let result = scheduling_executor.schedule_iox2_event(
            FrameworkTime::from_nanoseconds(0),
            &event_channel,
            EventId::new(12),
            25,
        );
        let _ = scheduled_tx.send(result);
    });
    release_tx.send(()).unwrap();
    scheduled_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("schedule call did not complete after the active step")
        .unwrap();
    scheduling_thread.join().unwrap();
    assert_eq!(
        observed_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("scheduled event was not delivered"),
        vec![(EventId::new(12), 25)]
    );
    let mut executor = Arc::try_unwrap(executor)
        .ok()
        .expect("scheduling thread released its executor handle");
    executor.stop().unwrap();
}

/// An injected runtime configuration reaches the graph node through TaskGraphBuilder.
#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn graph_config_injection_builds_simulation_node() {
    let event_channel = channel("config");
    let graph = TaskGraphBuilder::new()
        .with_iox2_config(Iox2GraphConfig {
            node_name: None,
            config: Some(iceoryx2::config::Config::default()),
        })
        .add_pool(1, |pool| {
            pool.add_callback(node(
                "event_consumer",
                Box::new(EventConsumer {
                    event: Iox2EventSubscriber::new(SubscriberConfig {
                        is_optional: true,
                        capacity: 1,
                        is_trigger: true,
                        keep_across_runs: true,
                        channel_name: event_channel,
                    }),
                    required: None,
                    observed: Arc::new(Mutex::new(Vec::new())),
                }),
            ))
        })
        .build();
    assert!(graph.is_ok());
}
