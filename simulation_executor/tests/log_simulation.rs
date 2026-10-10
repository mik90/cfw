#![cfg(feature = "log_simulation")]
use logging::{CapturePlan, OwnedLogEntry, ReplaySourcePlan, SortedLogStreamReader};
use simulation_executor::{LogSimulation, LogSimulationOptions, SimulationConfig, StepError};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use task::{
    CallbackSchedule, ChannelPlan, GraphBuilder, GraphPlan, InputSpan, LoanError, Publisher,
    message::MessageHeader, time::FrameworkTime,
};
use task_macros::task_callback;

fn at(n: i64) -> FrameworkTime {
    FrameworkTime::from_nanoseconds(n)
}
fn entry(n: i64, channel: &str, body: &[u8]) -> OwnedLogEntry {
    OwnedLogEntry {
        header: MessageHeader::new(at(n)),
        channel_name: channel.into(),
        serialized_body: body.into(),
    }
}
fn reader(entries: Vec<OwnedLogEntry>) -> SortedLogStreamReader {
    SortedLogStreamReader::from_entries(entries, HashMap::new()).unwrap()
}
struct Double;
#[task_callback]
impl Double {
    fn run(
        &self,
        #[capacity(8)] mut input: InputSpan<u64>,
        #[capacity(8)] output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        for value in input.drain() {
            output.publish(value.message * 2)?;
        }
        Ok(())
    }
}

#[test]
fn streaming_deadlines_prevent_skipping_inputs_and_eof_drains_downstream_work() {
    let mut input = ChannelPlan::new("input");
    let mut output = ChannelPlan::new("output");
    let declaration = Double::declare(&mut input, &mut output).unwrap();
    let source = ReplaySourcePlan::declare(&mut input, 1);
    let capture = CapturePlan::declare(&mut output, 8);
    let storage = GraphPlan::new((input, output)).allocate().unwrap();
    let input = storage.channels().0.build();
    let output = storage.channels().1.build();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_scheduled_callback(
        "double",
        CallbackSchedule::default().with_execution_duration(Duration::from_nanos(100)),
        || Ok(Double.bind(declaration, &input, &output)?),
    );
    let options = LogSimulationOptions {
        denylist: HashSet::from(["output".into()]),
        ..Default::default()
    };
    let log = reader(vec![
        entry(5, "input", b"3"),
        entry(0, "input", b"1"),
        entry(0, "output", b"999"),
        entry(0, "input", b"2"),
    ]);
    let mut simulation = LogSimulation::with_options(
        graph.build().unwrap(),
        log,
        [source.bind(&input).unwrap()],
        options,
    )
    .unwrap();
    let first = simulation.step().unwrap();
    assert_eq!(first.executed, [0]);
    assert_eq!(first.after, at(5));
    assert!(!simulation.input_exhausted());
    let second = simulation.step().unwrap();
    assert!(second.executed.is_empty());
    assert_eq!(second.after, at(100));
    assert!(!second.idle);
    assert!(simulation.input_exhausted());
    let steps = simulation.run_until_idle(4).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].executed, [0]);
    assert_eq!(simulation.current_time(), at(200));
    let messages = capture.bind(&output).unwrap().drain_to_vec().unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|(h, b)| (h.published_at, serde_json::from_slice::<u64>(b).unwrap()))
            .collect::<Vec<_>>(),
        [(at(0), 2), (at(0), 4), (at(100), 6)]
    );
}

#[test]
fn missing_sources_past_inputs_and_bad_payloads_are_reported() {
    assert!(
        LogSimulation::new(
            GraphBuilder::new().build().unwrap(),
            reader(vec![entry(0, "missing", b"1")]),
            []
        )
        .err()
        .unwrap()
        .to_string()
        .contains("no replay source")
    );
    for (body, start) in [
        (b"invalid".as_slice(), None),
        (b"1".as_slice(), Some(at(11))),
    ] {
        let mut input = ChannelPlan::<u64>::new("input");
        let source = ReplaySourcePlan::declare(&mut input, 1);
        let storage = GraphPlan::new(input).allocate().unwrap();
        let bindings = storage.channels().build();
        let options = LogSimulationOptions {
            simulation: start.map(|start_time| SimulationConfig {
                start_time,
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = LogSimulation::with_options(
            GraphBuilder::with_storage(&storage).build().unwrap(),
            reader(vec![entry(10, "input", body)]),
            [source.bind(&bindings).unwrap()],
            options,
        );
        if start.is_some() {
            assert!(result.is_err());
        } else {
            let mut simulation = result.unwrap();
            assert_eq!(simulation.current_time(), at(10));
            assert!(
                matches!(simulation.step(), Err(StepError::Action(reason)) if reason.contains("input"))
            );
            assert!(matches!(simulation.step(), Err(StepError::Poisoned)));
            assert!(!simulation.input_exhausted());
        }
    }
}

#[test]
fn empty_logs_and_infinite_timers_have_explicit_completion_semantics() {
    let mut empty =
        LogSimulation::new(GraphBuilder::new().build().unwrap(), reader(vec![]), []).unwrap();
    assert!(empty.input_exhausted());
    assert!(empty.step().unwrap().idle);
    let mut graph = GraphBuilder::new();
    graph.add_scheduled_callback(
        "periodic",
        CallbackSchedule::periodic(Duration::from_nanos(5))
            .with_execution_duration(Duration::from_nanos(1)),
        || Ok(|_| Ok(())),
    );
    let mut simulation = LogSimulation::with_options(
        graph.build().unwrap(),
        reader(vec![]),
        [],
        LogSimulationOptions {
            eof: simulation_executor::EofPolicy::Continue,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        simulation.run_until_idle(4),
        Err(StepError::StepLimitExceeded)
    ));
}

#[test]
fn eof_drain_tail_and_cancellation_bound_periodic_work() {
    use simulation_executor::{CompletionReason, EofPolicy};
    for policy in [
        EofPolicy::Drain,
        EofPolicy::Tail(Duration::from_nanos(12)),
        EofPolicy::Continue,
    ] {
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = observed.clone();
        let mut graph = GraphBuilder::new();
        graph.add_scheduled_callback(
            "timer",
            CallbackSchedule::periodic(Duration::from_nanos(5))
                .with_execution_duration(Duration::from_nanos(1)),
            || {
                Ok(move |time| {
                    seen.lock().unwrap().push(time);
                    Ok(())
                })
            },
        );
        let mut replay = LogSimulation::with_options(
            graph.build().unwrap(),
            reader(vec![]),
            [],
            LogSimulationOptions {
                eof: policy,
                ..Default::default()
            },
        )
        .unwrap();
        let completion = replay.run_until(20, || true).unwrap();
        assert!(completion.input_exhausted);
        match policy {
            EofPolicy::Drain => {
                assert_eq!(completion.reason, CompletionReason::Drained);
                assert!(observed.lock().unwrap().is_empty());
            }
            EofPolicy::Tail(_) => {
                assert_eq!(completion.reason, CompletionReason::TailDrained);
                assert!(completion.at >= at(12));
                let observed = observed.lock().unwrap();
                assert!(!observed.is_empty());
                assert!(observed.iter().all(|time| *time < at(12)));
            }
            EofPolicy::Continue => {
                assert_eq!(completion.reason, CompletionReason::StepBudgetExhausted);
                assert_eq!(
                    replay.run_until(20, || false).unwrap().reason,
                    CompletionReason::Cancelled
                );
            }
        }
    }
}

#[test]
fn eof_drain_finishes_inflight_work_and_bounds_feedback_cycles() {
    use simulation_executor::{CompletionReason, EofPolicy};
    let mut graph = GraphBuilder::new();
    graph.add_scheduled_callback(
        "slow",
        CallbackSchedule::periodic(Duration::from_nanos(5))
            .with_execution_duration(Duration::from_nanos(20)),
        || Ok(|_| Ok(())),
    );
    let mut replay = LogSimulation::with_options(
        graph.build().unwrap(),
        reader(vec![]),
        [],
        LogSimulationOptions {
            eof: EofPolicy::Tail(Duration::from_nanos(10)),
            ..Default::default()
        },
    )
    .unwrap();
    let completion = replay.run_until(20, || true).unwrap();
    assert_eq!(completion.reason, CompletionReason::TailDrained);
    assert_eq!(completion.at, at(25));

    let mut plan = task::automatic::NamedPlan::default();
    let mut task = task::automatic::TaskRegistration::new(
        "feedback",
        Double,
        CallbackSchedule::default().with_execution_duration(Duration::from_nanos(1)),
    );
    task.input_channel("input", "loop")
        .output_channel("output", "loop");
    let task = task.register(&mut plan).unwrap();
    let source = ReplaySourcePlan::declare(plan.native::<u64>("loop").unwrap(), 1);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut graph = storage.graph_builder();
    task.add_to_graph(&mut graph, &bindings);
    let mut replay = LogSimulation::new(
        graph.build().unwrap(),
        reader(vec![entry(0, "loop", b"1")]),
        [source
            .bind(bindings.native::<u64>("loop").unwrap())
            .unwrap()],
    )
    .unwrap();
    let completion = replay.run_until(16, || true).unwrap();
    assert!(completion.input_exhausted);
    assert_eq!(completion.reason, CompletionReason::StepBudgetExhausted);
}

#[test]
fn stream_action_errors_and_nonadvancing_deadlines_poison_the_session() {
    let mut simulation =
        simulation_executor::SimulationState::new(GraphBuilder::new().build().unwrap()).unwrap();
    simulation
        .schedule_stream(at(0), |time| Ok(Some(time)))
        .unwrap();
    assert!(matches!(simulation.step(), Err(StepError::Action(_))));
    assert!(matches!(simulation.step(), Err(StepError::Poisoned)));
}

fn with_graph<R>(
    run: impl for<'a> FnOnce(
        task::BuiltGraph<'a>,
        logging::ReplaySource<'a>,
        logging::Capture<'a>,
        logging::Capture<'a>,
    ) -> R,
) -> R {
    let mut input = ChannelPlan::new("input");
    let mut output = ChannelPlan::new("output");
    let declaration = Double::declare(&mut input, &mut output).unwrap();
    let source = ReplaySourcePlan::declare(&mut input, 1);
    let input_capture = CapturePlan::declare(&mut input, 8);
    let output_capture = CapturePlan::declare(&mut output, 8);
    let storage = GraphPlan::new((input, output)).allocate().unwrap();
    let input = storage.channels().0.build();
    let output = storage.channels().1.build();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_scheduled_callback(
        "double",
        CallbackSchedule::default().with_execution_duration(Duration::from_nanos(3)),
        || Ok(Double.bind(declaration, &input, &output)?),
    );
    run(
        graph.build().unwrap(),
        source.bind(&input).unwrap(),
        input_capture.bind(&input).unwrap(),
        output_capture.bind(&output).unwrap(),
    )
}

#[test]
#[cfg_attr(miri, ignore = "sorted JSON reader requires filesystem")]
fn record_sort_and_simulate_current_format_end_to_end() {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    #[derive(Clone, Default)]
    struct Bytes(Arc<Mutex<Vec<u8>>>);
    impl Write for Bytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Bytes::default();
    with_graph(|graph, source, input_capture, output_capture| {
        let recorder = logging::ExecutionRecorder::new(8);
        let graph = recorder.attach(graph).unwrap();
        let log = logging::LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            vec![input_capture, output_capture],
        )
        .with_recording(recorder)
        .unwrap();
        let mut simulation = simulation_executor::SimulationState::with_config(
            graph,
            SimulationConfig {
                start_time: at(10),
                ..Default::default()
            },
        )
        .unwrap();
        let source = Arc::new(Mutex::new(source));
        for (time, body) in [(10, b"2".as_slice()), (20, b"3".as_slice())] {
            let source = source.clone();
            simulation
                .schedule_at(at(time), move |stamp| {
                    source
                        .lock()
                        .unwrap()
                        .inject(MessageHeader::new(stamp), body)
                        .map_err(|e| e.to_string())
                })
                .unwrap();
        }
        simulation.run_until_idle(10).unwrap();
        log.finish().unwrap();
    });
    let log = SortedLogStreamReader::from_reader(bytes.0.lock().unwrap().as_slice(), 1).unwrap();
    with_graph(|graph, source, _input_capture, mut output_capture| {
        let mut simulation = LogSimulation::with_options(
            graph,
            log,
            [source],
            LogSimulationOptions {
                denylist: HashSet::from(["output".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        simulation.run_until_idle(10).unwrap();
        assert!(simulation.input_exhausted());
        let messages = output_capture.drain_to_vec().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|(h, b)| (h.published_at, serde_json::from_slice::<u64>(b).unwrap()))
                .collect::<Vec<_>>(),
            [(at(10), 4), (at(20), 6)]
        );
    });
}

fn event_reader(channel: &str, ordinal: usize) -> SortedLogStreamReader {
    use task::recording::*;
    let descriptor = ExecutionDescriptor {
        callbacks: vec![CallbackDescriptor {
            recording_mode: RecordingMode::Full,
            name: "absent".into(),
            endpoints: vec![EndpointDescriptor {
                ordinal: 0,
                channel: channel.into(),
                direction: Direction::Received,
                transport: Transport::Event,
                payload_type: "()".into(),
                publisher_index: None,
            }],
        }],
        logged_channels: vec![],
    };
    let event = ObservedEvent {
        callback_id: task::string_interner::CallbackId::from_index(0).unwrap(),
        observed_at: at(10),
        event: LoggedEvent {
            ordinal,
            event_id: 0,
            count: 1,
        },
    };
    SortedLogStreamReader::from_entries(
        vec![entry(
            10,
            EXECUTION_EVENT_CHANNEL,
            &serde_json::to_vec(&event).unwrap(),
        )],
        descriptor_artifacts(&descriptor),
    )
    .unwrap()
}
fn descriptor_artifacts(
    descriptor: &task::recording::ExecutionDescriptor,
) -> HashMap<String, Vec<u8>> {
    let mut tables = logging::InternTables::default();
    for callback in &descriptor.callbacks {
        tables.callbacks.intern(&callback.name);
        for port in &callback.endpoints {
            tables.channels.intern(&port.channel);
        }
    }
    HashMap::from([
        (
            task::recording::EXECUTION_LOG_DESCRIPTOR_ARTIFACT.into(),
            serde_json::to_vec(descriptor).unwrap(),
        ),
        (
            logging::INTERN_TABLES_ARTIFACT.into(),
            serde_json::to_vec(&tables).unwrap(),
        ),
    ])
}
#[test]
fn events_validate_descriptors_targets_and_channel_filters() {
    assert!(
        LogSimulation::new(
            GraphBuilder::new().build().unwrap(),
            event_reader("events", 9),
            []
        )
        .err()
        .unwrap()
        .to_string()
        .contains("ordinal")
    );
    let mut invalid = LogSimulation::new(
        GraphBuilder::new().build().unwrap(),
        event_reader("events", 0),
        [],
    )
    .unwrap();
    assert!(matches!(invalid.step(), Err(StepError::Action(_))));
    assert!(matches!(invalid.step(), Err(StepError::Poisoned)));
    let mut denied = LogSimulation::with_options(
        GraphBuilder::new().build().unwrap(),
        event_reader("events", 0),
        [],
        LogSimulationOptions {
            denylist: HashSet::from(["events".into()]),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(denied.input_exhausted());
    assert!(denied.step().unwrap().idle);
}

#[cfg(feature = "iceoryx2")]
mod ipc {
    use super::*;
    use std::sync::{Arc, Mutex};
    use task::{
        Context, RequiredInput,
        iox2::{Iox2ChannelPlan, Iox2Event, Iox2OptionalInput, Iox2Runtime},
        recording::*,
    };
    type Trace = Arc<Mutex<Vec<(&'static str, FrameworkTime, u64, u64)>>>;
    struct Observe {
        name: &'static str,
        trace: Trace,
    }
    #[task_callback]
    impl Observe {
        fn run(
            &self,
            event: Iox2Event,
            #[trigger(false)] gate: RequiredInput<u64>,
            data: Iox2OptionalInput<u64>,
            context: &Context,
        ) {
            assert_eq!(data.value(), Some(&42));
            self.trace
                .lock()
                .unwrap()
                .push((self.name, context.now(), event.count(), *gate));
        }
    }
    #[test]
    #[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
    fn per_recipient_events_survive_gating_without_multiplying_fanout() {
        let runtime = Iox2Runtime::new().unwrap();
        let name = format!("log_sim_recipients_{}", std::process::id());
        let mut events = Iox2ChannelPlan::<u64>::new(&name, &runtime);
        let mut gate = ChannelPlan::<u64>::new("gate");
        let a = ObserveDeclaration::from_keys(
            events.events(4),
            gate.subscriber(1),
            events.subscriber(1),
        );
        let b = ObserveDeclaration::from_keys(
            events.events(4),
            gate.subscriber(1),
            events.subscriber(1),
        );
        let data_source = events.publisher(1);
        let source = ReplaySourcePlan::declare(&mut gate, 1);
        let storage = GraphPlan::new((events, gate)).allocate().unwrap();
        let events = storage.channels().0.build().unwrap();
        let gate = storage.channels().1.build();
        let trace: Trace = Default::default();
        let mut graph = GraphBuilder::with_storage(&storage);
        // Current graph indices differ from the log: resolve by callback name.
        graph.add_scheduled_callback(
            "b",
            CallbackSchedule::default().with_execution_duration(Duration::from_nanos(2)),
            || {
                Ok(Observe {
                    name: "b",
                    trace: trace.clone(),
                }
                .bind(b, &events, &gate, &events)?)
            },
        );
        graph.add_scheduled_callback(
            "a",
            CallbackSchedule::default().with_execution_duration(Duration::from_nanos(2)),
            || {
                Ok(Observe {
                    name: "a",
                    trace: trace.clone(),
                }
                .bind(a, &events, &gate, &events)?)
            },
        );
        let descriptor = ExecutionDescriptor {
            callbacks: ["a", "b"]
                .into_iter()
                .map(|callback| CallbackDescriptor {
                    recording_mode: RecordingMode::Full,
                    name: callback.into(),
                    endpoints: vec![
                        EndpointDescriptor {
                            ordinal: 0,
                            channel: name.clone(),
                            direction: Direction::Received,
                            transport: Transport::Event,
                            payload_type: "()".into(),
                            publisher_index: None,
                        },
                        EndpointDescriptor {
                            ordinal: 1,
                            channel: "gate".into(),
                            direction: Direction::Received,
                            transport: Transport::Native,
                            payload_type: "u64".into(),
                            publisher_index: None,
                        },
                        EndpointDescriptor {
                            ordinal: 2,
                            channel: name.clone(),
                            direction: Direction::Received,
                            transport: Transport::Ipc,
                            payload_type: "u64".into(),
                            publisher_index: None,
                        },
                    ],
                })
                .collect(),
            logged_channels: vec!["gate".into(), name.clone()],
        };
        let mut rows = Vec::new();
        for callback_index in 0..2 {
            rows.push(entry(
                10,
                EXECUTION_EVENT_CHANNEL,
                &serde_json::to_vec(&ObservedEvent {
                    callback_id: task::string_interner::CallbackId::from_index(callback_index)
                        .unwrap(),
                    observed_at: at(10),
                    event: LoggedEvent {
                        ordinal: 0,
                        event_id: 0,
                        count: 3,
                    },
                })
                .unwrap(),
            ));
        }
        rows.push(entry(10, &name, b"42"));
        rows.push(entry(15, "gate", b"7"));
        let log =
            SortedLogStreamReader::from_entries(rows, descriptor_artifacts(&descriptor)).unwrap();
        let mut simulation = LogSimulation::with_options(
            graph.build().unwrap(),
            log,
            [
                source.bind(&gate).unwrap(),
                logging::ReplaySource::ipc(events.take_publisher(&data_source).unwrap()),
            ],
            LogSimulationOptions {
                simulation: Some(SimulationConfig {
                    start_time: at(10),
                    virtual_pool_threads: vec![2],
                    node_executor_thread_count: 2,
                    poll_external_events: false,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(simulation.step().unwrap().executed.is_empty());
        assert_eq!(simulation.current_time(), at(15));
        assert!(trace.lock().unwrap().is_empty());
        simulation.run_until_idle(5).unwrap();
        let mut actual = trace.lock().unwrap().clone();
        actual.sort_by_key(|value| value.0);
        assert_eq!(actual, [("a", at(15), 3, 7), ("b", at(15), 3, 7)]);
    }
}
