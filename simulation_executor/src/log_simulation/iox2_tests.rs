use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use iceoryx2::prelude::ZeroCopySend;
use logging::log_file::LogFileWriter;
use logging::log_file_json::JsonLogFileWriter;
use task::callback::{Callback, CallbackNode, PubOrSub, PubOrSubMut};
use task::context::Context;
use task::executor::{ExecutorParams, ExecutorStopSignal};
use task::iox2::{Iox2Event, Iox2EventSubscriber, Iox2OptionalInput, Iox2Subscriber};
use task::message::MessageHeader;
use task::subscriber::{Subscriber, SubscriberConfig};
use task::task_graph_builder::{BuiltTaskGraph, TaskGraphBuilder};
use task::time::FrameworkTime;

use crate::SimulationConfig;
use crate::state::SimulationState;

use super::{AtomicFrameworkTime, LogSimulationBuildStep, SortedLogStreamReader};

type Iox2Observation = (FrameworkTime, Option<(u64, FrameworkTime)>, u64);
type Iox2Observations = Arc<Mutex<Vec<Iox2Observation>>>;

struct Iox2Reader {
    data: Iox2Subscriber<u64>,
    event: Option<Iox2EventSubscriber>,
    observed: Iox2Observations,
}

impl Callback for Iox2Reader {
    fn run(&mut self, ctx: &Context) {
        let data = Iox2OptionalInput::new(&self.data);
        let message = data
            .value()
            .copied()
            .zip(data.header().map(|header| header.published_at));
        let events = self
            .event
            .as_ref()
            .map_or(0, |subscriber| Iox2Event::new(subscriber).count());
        self.observed
            .lock()
            .unwrap()
            .push((ctx.now, message, events));
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Subscriber(&self.data));
        if let Some(event) = &self.event {
            f(PubOrSub::Subscriber(event));
        }
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Subscriber(&mut self.data));
        if let Some(event) = &mut self.event {
            f(PubOrSubMut::Subscriber(event));
        }
    }
}

struct NativeReader {
    data: Subscriber<u64>,
    observed: Arc<Mutex<Vec<Option<u64>>>>,
}

struct NativeStringReader {
    data: Subscriber<String>,
    observed: Arc<Mutex<Vec<Option<String>>>>,
}

#[repr(C)]
#[derive(Debug, ZeroCopySend)]
struct UnregisteredPayload {
    value: u64,
}

struct UnregisteredIox2Reader(Iox2Subscriber<UnregisteredPayload>);

impl Callback for UnregisteredIox2Reader {
    fn run(&mut self, _ctx: &Context) {
        let _ = Iox2OptionalInput::new(&self.0)
            .value()
            .map(|payload| payload.value);
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Subscriber(&self.0));
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Subscriber(&mut self.0));
    }
}

impl Callback for NativeStringReader {
    fn run(&mut self, _ctx: &Context) {
        self.observed.lock().unwrap().push(
            self.data
                .read_buffer()
                .front()
                .map(|message| message.message.clone()),
        );
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Subscriber(&self.data));
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Subscriber(&mut self.data));
    }
}

impl Callback for NativeReader {
    fn run(&mut self, _ctx: &Context) {
        self.observed.lock().unwrap().push(
            self.data
                .read_buffer()
                .front()
                .map(|message| message.message),
        );
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        f(PubOrSub::Subscriber(&self.data));
    }

    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        f(PubOrSubMut::Subscriber(&mut self.data));
    }
}

fn subscriber_config(channel: &str) -> SubscriberConfig {
    SubscriberConfig {
        is_optional: true,
        capacity: 4,
        is_trigger: false,
        keep_across_runs: true,
        channel_name: channel.into(),
    }
}

fn reader_node(name: &str, callback: Box<dyn Callback>, periodic: bool) -> CallbackNode {
    let mut node = CallbackNode::new_named(callback, name.into());
    node.set_execution_duration_callback(Box::new(|| Duration::ZERO));
    if periodic {
        node.set_execution_time_callback(Box::new(|now| Some(now + Duration::from_nanos(100))));
    }
    node
}

fn log_build_step(
    entries: &[(FrameworkTime, &str, u64)],
) -> (LogSimulationBuildStep, FrameworkTime) {
    let mut bytes = Vec::new();
    {
        let mut writer = JsonLogFileWriter::new(&mut bytes);
        for &(at, channel, value) in entries {
            let mut body = Vec::new();
            task::loggable::Loggable::serialize(&value, &mut body).unwrap();
            writer
                .store_message(channel, &MessageHeader::new(at), &body)
                .unwrap();
        }
    }
    log_build_step_from_bytes(&bytes)
}

fn log_build_step_from_bytes(bytes: &[u8]) -> (LogSimulationBuildStep, FrameworkTime) {
    let mut reader = SortedLogStreamReader::from_reader(bytes, 16).unwrap();
    let start = reader.peek_time().unwrap();
    (
        LogSimulationBuildStep {
            reader: Arc::new(Mutex::new(Some(reader))),
            next_time_ns: Arc::new(AtomicFrameworkTime::new(start)),
            denylist: HashSet::new(),
            stop_signal_cell: Arc::new(OnceLock::<Arc<dyn ExecutorStopSignal>>::new()),
            first_time: start,
        },
        start,
    )
}

fn state_from_graph(mut graph: BuiltTaskGraph, start: FrameworkTime) -> SimulationState {
    let params = ExecutorParams::new(std::mem::take(&mut graph.pools))
        .with_iox2_context(graph.iox2_context.take());
    let mut state = SimulationState::try_new_with(SimulationConfig {
        start_time: start,
        executor_params: params,
        node_executor_thread_count: 3,
    })
    .unwrap();
    state.start();
    state
}

#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn iox2_log_replay_preserves_header_and_does_not_notify() {
    let channel = format!("sim_log_iox2_{}_data", std::process::id());
    let logged_at = FrameworkTime::from_nanoseconds(1_000);
    let first = Arc::new(Mutex::new(Vec::new()));
    let second = Arc::new(Mutex::new(Vec::new()));
    let no_trigger = Arc::new(Mutex::new(Vec::new()));
    let make_reader = |observed: Iox2Observations, event: bool| {
        Box::new(Iox2Reader {
            data: Iox2Subscriber::new(subscriber_config(&channel)),
            event: event.then(|| {
                Iox2EventSubscriber::new(SubscriberConfig {
                    is_trigger: true,
                    ..subscriber_config(&channel)
                })
            }),
            observed,
        }) as Box<dyn Callback>
    };
    let (log_step, start) = log_build_step(&[(logged_at, &channel, 42)]);
    let mut builder = TaskGraphBuilder::new()
        .add_pool(4, |pool| {
            pool.add_callback(reader_node("first", make_reader(first.clone(), true), true))
                .add_callback(reader_node(
                    "second",
                    make_reader(second.clone(), false),
                    true,
                ))
                .add_callback(reader_node(
                    "event_only",
                    make_reader(no_trigger.clone(), true),
                    false,
                ))
        })
        .add_build_step(Box::new(log_step));
    builder
        .channel_registry_mut()
        .register_channel::<u64>(channel.clone());
    let graph = builder.build().expect("iox2 log graph builds");
    let mut state = state_from_graph(graph, start);
    state.step().unwrap();
    state.step().unwrap();

    for observed in [&first, &second] {
        let observed = observed.lock().unwrap();
        assert_eq!(observed[0].1, None);
        assert!(
            observed.iter().any(|&(ran_at, message, events)| {
                ran_at > logged_at && message == Some((42, logged_at)) && events == 0
            }),
            "replayed iox2 sample/header was not observed: {observed:?}"
        );
    }
    assert!(
        no_trigger.lock().unwrap().is_empty(),
        "data must not trigger the event-only reader"
    );
}

#[test]
#[cfg_attr(miri, ignore = "iceoryx2 IPC not guaranteed to work under Miri")]
fn iox2_log_replay_chooses_transport_per_channel() {
    let prefix = format!("sim_log_mixed_{}", std::process::id());
    let native_channel = format!("{prefix}_native");
    let iox2_channel = format!("{prefix}_iox2");
    let logged_at = FrameworkTime::from_nanoseconds(2_000);
    let native_observed = Arc::new(Mutex::new(Vec::new()));
    let iox2_observed = Arc::new(Mutex::new(Vec::new()));
    let (log_step, start) = log_build_step(&[
        (logged_at, &native_channel, 13),
        (logged_at, &iox2_channel, 55),
    ]);
    let mut builder = TaskGraphBuilder::new()
        .add_pool(3, |pool| {
            pool.add_callback(reader_node(
                "native_reader",
                Box::new(NativeReader {
                    data: Subscriber::new(subscriber_config(&native_channel)),
                    observed: native_observed.clone(),
                }),
                true,
            ))
            .add_callback(reader_node(
                "iox2_reader",
                Box::new(Iox2Reader {
                    data: Iox2Subscriber::new(subscriber_config(&iox2_channel)),
                    event: None,
                    observed: iox2_observed.clone(),
                }),
                true,
            ))
        })
        .add_build_step(Box::new(log_step));
    builder
        .channel_registry_mut()
        .register_channel::<u64>(native_channel);
    builder
        .channel_registry_mut()
        .register_channel::<u64>(iox2_channel);
    let graph = builder.build().expect("native and iox2 channels coexist");
    let mut state = state_from_graph(graph, start);
    state.step().unwrap();
    state.step().unwrap();

    assert!(native_observed.lock().unwrap().contains(&Some(13)));
    assert!(
        iox2_observed
            .lock()
            .unwrap()
            .iter()
            .any(|observed| observed.1 == Some((55, logged_at)))
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "log simulation reads its input through a temporary file"
)]
fn native_string_log_replay_needs_no_zero_copy_payload() {
    let channel = format!("sim_log_string_{}", std::process::id());
    let logged_at = FrameworkTime::from_nanoseconds(3_000);
    let mut bytes = Vec::new();
    {
        let mut writer = JsonLogFileWriter::new(&mut bytes);
        let mut body = Vec::new();
        task::loggable::Loggable::serialize(&String::from("hello"), &mut body).unwrap();
        writer
            .store_message(&channel, &MessageHeader::new(logged_at), &body)
            .unwrap();
    }
    let (log_step, start) = log_build_step_from_bytes(&bytes);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut builder = TaskGraphBuilder::new()
        .add_pool(2, |pool| {
            pool.add_callback(reader_node(
                "native_string_reader",
                Box::new(NativeStringReader {
                    data: Subscriber::new(subscriber_config(&channel)),
                    observed: observed.clone(),
                }),
                true,
            ))
        })
        .add_build_step(Box::new(log_step));
    builder
        .channel_registry_mut()
        .register_channel::<String>(channel);
    let mut state = state_from_graph(builder.build().unwrap(), start);
    state.step().unwrap();
    state.step().unwrap();
    assert!(
        observed
            .lock()
            .unwrap()
            .contains(&Some(String::from("hello")))
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "log simulation reads its input through a temporary file"
)]
fn iox2_log_replay_names_unregistered_channel() {
    let channel = format!("sim_log_unregistered_{}", std::process::id());
    let (log_step, _) = log_build_step(&[(FrameworkTime::from_nanoseconds(4_000), &channel, 1)]);
    let graph = TaskGraphBuilder::new()
        .add_pool(1, |pool| {
            pool.add_callback(reader_node(
                "unregistered_iox2_reader",
                Box::new(UnregisteredIox2Reader(Iox2Subscriber::new(
                    subscriber_config(&channel),
                ))),
                false,
            ))
        })
        .add_build_step(Box::new(log_step))
        .build();
    let error = graph.expect_err("unregistered iox2 payload cannot be replayed");
    assert!(
        error.to_string().contains(&channel),
        "diagnostic omitted channel name: {error}"
    );
}
