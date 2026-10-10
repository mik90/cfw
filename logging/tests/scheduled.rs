use logging::{CapturePlan, FlushEventPlan, LogFileWriter, LoggingScope};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use task::{
    CallbackSchedule, ChannelPlan, GraphBuilder, GraphPlan, LoanError, Publisher,
    message::MessageHeader, time::FrameworkTime,
};
use task_macros::task_callback;

#[derive(Default)]
struct Wire(u64);
impl task::loggable::Loggable for Wire {
    type Context<'a> = ();
    fn serialize(
        &self,
        writer: &mut dyn std::io::Write,
    ) -> Result<(), task::loggable::SerializeError> {
        writer.write_all(&self.0.to_le_bytes())?;
        Ok(())
    }
    fn deserialize_with_ctx<'a>(
        bytes: &[u8],
        _: (),
    ) -> Result<Self, task::loggable::DeserializeError>
    where
        Self: 'a,
    {
        Ok(Self(u64::from_le_bytes(bytes.try_into()?)))
    }
}
#[derive(Clone, Default)]
struct Writer(Arc<Mutex<State>>);
#[derive(Default)]
struct State {
    messages: Vec<(String, MessageHeader, Vec<u8>)>,
    artifacts: Vec<(String, Vec<u8>)>,
    flushes: usize,
    fail: bool,
}
impl LogFileWriter for Writer {
    fn store_message(
        &mut self,
        channel: &str,
        header: &MessageHeader,
        body: &[u8],
    ) -> Result<(), logging::BoxedLogError> {
        let mut state = self.0.lock().unwrap();
        if state.fail {
            return Err("scheduled writer failure".into());
        }
        state
            .messages
            .push((channel.into(), *header, body.to_vec()));
        Ok(())
    }
    fn write_artifact(&mut self, name: &str, body: &[u8]) -> Result<(), logging::BoxedLogError> {
        self.0
            .lock()
            .unwrap()
            .artifacts
            .push((name.into(), body.into()));
        Ok(())
    }
    fn flush(&mut self) -> Result<(), logging::BoxedLogError> {
        self.0.lock().unwrap().flushes += 1;
        Ok(())
    }
}
struct Source;
#[task_callback]
impl Source {
    fn run(&self, output: &mut Publisher<Wire>) -> Result<(), LoanError> {
        output.publish(Wire(42))
    }
}
fn at(n: i64) -> FrameworkTime {
    FrameworkTime::from_nanoseconds(n)
}

#[test]
fn periodic_flush_uses_executor_time_and_does_not_run_at_start() {
    let mut plan = ChannelPlan::new("data");
    let source = Source::declare(&mut plan).unwrap();
    let capture = CapturePlan::declare(&mut plan, 2);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut builder = GraphBuilder::with_storage(&storage);
    builder.add_scheduled_callback(
        "source",
        CallbackSchedule::on_start().with_execution_duration(Duration::ZERO),
        || Ok(Source.bind(source, &bindings)?),
    );
    let writer = Writer::default();
    let logger = LoggingScope::new(writer.clone(), vec![capture.bind(&bindings).unwrap()], 3);
    assert_eq!(logger.shard_count(), 1);
    {
        let graph = logger
            .attach_periodic(builder.build().unwrap(), Duration::from_nanos(10))
            .unwrap();
        let mut sim = simulation_executor::SimulationState::new(graph).unwrap();
        assert_eq!(sim.step().unwrap().executed, [0]);
        assert!(writer.0.lock().unwrap().messages.is_empty());
        let flushed = sim.step().unwrap();
        assert_eq!(flushed.executed, [1]);
        assert_eq!(flushed.before, at(10));
        let state = writer.0.lock().unwrap();
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].1.published_at, at(0));
        assert_eq!(state.flushes, 1);
    }
    logger.finish().unwrap();
}

#[test]
fn event_mode_waits_for_explicit_pulses_and_final_finish_drains_the_tail() {
    let mut data = ChannelPlan::new("data");
    let mut events = ChannelPlan::new("flush");
    let publisher = data.publisher(1);
    let pulse = events.publisher(1);
    let capture = CapturePlan::declare(&mut data, 2);
    let triggers = FlushEventPlan::declare(&mut events, 1);
    let storage = GraphPlan::new((data, events)).allocate().unwrap();
    let data = storage.channels().0.build();
    let events = storage.channels().1.build();
    let mut publisher = data.take_publisher(&publisher).unwrap();
    let mut pulse = events.take_publisher(&pulse).unwrap();
    let writer = Writer::default();
    let logger = LoggingScope::new(writer.clone(), vec![capture.bind(&data).unwrap()], 1);
    {
        let graph = logger
            .attach_event(
                GraphBuilder::with_storage(&storage).build().unwrap(),
                triggers.bind(&events).unwrap(),
            )
            .unwrap();
        let mut sim = simulation_executor::SimulationState::new(graph).unwrap();
        publisher.publish(Wire(10)).unwrap();
        publisher.flush(at(0));
        assert!(sim.step().unwrap().executed.is_empty());
        assert!(writer.0.lock().unwrap().messages.is_empty());
        pulse.publish(()).unwrap();
        pulse.flush(at(1));
        assert_eq!(sim.step().unwrap().executed, [0]);
        assert_eq!(writer.0.lock().unwrap().messages.len(), 1);
        publisher.publish(Wire(11)).unwrap();
        publisher.flush(at(2));
        assert!(sim.step().unwrap().executed.is_empty());
    }
    logger.finish().unwrap();
    assert_eq!(writer.0.lock().unwrap().messages.len(), 2);
}

#[test]
fn event_pulses_fan_out_to_shards_and_coalesce_without_duplicate_messages() {
    let mut a = ChannelPlan::<Wire>::new("a");
    let mut b = ChannelPlan::<Wire>::new("b");
    let mut events = ChannelPlan::new("flush");
    let a_key = a.publisher(1);
    let b_key = b.publisher(1);
    let pulse = events.publisher(2);
    let a_log = CapturePlan::declare(&mut a, 1);
    let b_log = CapturePlan::declare(&mut b, 1);
    let triggers = FlushEventPlan::declare(&mut events, 2);
    let storage = GraphPlan::new((a, (b, events))).allocate().unwrap();
    let a = storage.channels().0.build();
    let b = storage.channels().1.0.build();
    let events = storage.channels().1.1.build();
    let mut a_pub = a.take_publisher(&a_key).unwrap();
    let mut b_pub = b.take_publisher(&b_key).unwrap();
    let mut pulse = events.take_publisher(&pulse).unwrap();
    let writer = Writer::default();
    let logger = LoggingScope::new(
        writer.clone(),
        vec![a_log.bind(&a).unwrap(), b_log.bind(&b).unwrap()],
        2,
    );
    {
        let graph = logger
            .attach_event(
                GraphBuilder::with_storage(&storage).build().unwrap(),
                triggers.bind(&events).unwrap(),
            )
            .unwrap();
        let mut sim = simulation_executor::SimulationState::with_config(
            graph,
            simulation_executor::SimulationConfig {
                virtual_pool_threads: vec![2],
                node_executor_thread_count: 2,
                ..Default::default()
            },
        )
        .unwrap();
        a_pub.publish(Wire(1)).unwrap();
        a_pub.flush(at(0));
        b_pub.publish(Wire(2)).unwrap();
        b_pub.flush(at(0));
        pulse.publish(()).unwrap();
        pulse.publish(()).unwrap();
        pulse.flush(at(1));
        assert_eq!(sim.step().unwrap().executed, [0, 1]);
        assert!(sim.step().unwrap().executed.is_empty());
        assert_eq!(writer.0.lock().unwrap().messages.len(), 2);
    }
    logger.finish().unwrap();
    assert_eq!(writer.0.lock().unwrap().messages.len(), 2);
}

#[test]
fn scheduled_errors_do_not_stop_workload_and_remain_sticky_after_recovery() {
    let mut plan = ChannelPlan::<Wire>::new("data");
    let publisher = plan.publisher(1);
    let capture = CapturePlan::declare(&mut plan, 1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut publisher = bindings.take_publisher(&publisher).unwrap();
    let writer = Writer::default();
    let logger = LoggingScope::new(writer.clone(), vec![capture.bind(&bindings).unwrap()], 1);
    let status = logger.status();
    {
        let graph = logger
            .attach_periodic(
                GraphBuilder::with_storage(&storage).build().unwrap(),
                Duration::from_nanos(1),
            )
            .unwrap();
        let mut sim = simulation_executor::SimulationState::new(graph).unwrap();
        writer.0.lock().unwrap().fail = true;
        publisher.publish(Wire(1)).unwrap();
        publisher.flush(at(0));
        assert!(sim.step().unwrap().executed.is_empty());
        assert_eq!(sim.step().unwrap().executed, [0]);
        assert!(!status.errors().is_empty());
        let diagnostics = status.diagnostics();
        assert_eq!(diagnostics[0].0, 0);
        assert_eq!(diagnostics[0].1.channel.as_deref(), Some("data"));
        assert_eq!(diagnostics[0].1.at, Some(at(1)));
        assert_eq!(diagnostics[0].1.kind, logging::DiagnosticKind::Write);
        writer.0.lock().unwrap().fail = false;
        publisher.publish(Wire(2)).unwrap();
        publisher.flush(at(2));
        assert_eq!(sim.step().unwrap().executed, [0]);
    }
    assert!(logger.finish().is_err());
    let state = writer.0.lock().unwrap();
    assert_eq!(state.messages.len(), 1);
    #[cfg(feature = "serde")]
    assert!(
        state
            .artifacts
            .iter()
            .any(|(name, _)| name == logging::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT)
    );
}

#[test]
fn invalid_schedule_trigger_count_and_duplicate_attachment_are_rejected() {
    assert_eq!(LoggingScope::planned_shard_count(0, 0), 1);
    assert_eq!(LoggingScope::planned_shard_count(2, 5), 2);
    assert_eq!(LoggingScope::planned_shard_count(5, 2), 2);
    let logger = LoggingScope::new(Writer::default(), vec![], 0);
    assert!(
        logger
            .attach_periodic(GraphBuilder::new().build().unwrap(), Duration::ZERO)
            .is_err()
    );
    assert!(
        logger
            .attach_event(GraphBuilder::new().build().unwrap(), vec![])
            .is_err()
    );
    let graph = logger
        .attach_periodic(
            GraphBuilder::new().build().unwrap(),
            Duration::from_nanos(1),
        )
        .unwrap();
    drop(graph);
    assert!(
        logger
            .attach_periodic(
                GraphBuilder::new().build().unwrap(),
                Duration::from_nanos(1)
            )
            .is_err()
    );
    logger.finish().unwrap();
}

#[cfg(feature = "serde")]
#[test]
fn shards_share_a_writer_and_record_only_workload_callbacks_once() {
    use task::recording::{
        EXECUTION_LOG_CHANNEL, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, ExecutionDescriptor,
    };
    let mut a = ChannelPlan::new("a");
    let mut b = ChannelPlan::new("b");
    let a_source = Source::declare(&mut a).unwrap();
    let b_source = Source::declare(&mut b).unwrap();
    let a_log = CapturePlan::declare(&mut a, 2);
    let b_log = CapturePlan::declare(&mut b, 2);
    let storage = GraphPlan::new((a, b)).allocate().unwrap();
    let a = storage.channels().0.build();
    let b = storage.channels().1.build();
    let mut builder = GraphBuilder::with_storage(&storage);
    let schedule = CallbackSchedule::on_start().with_execution_duration(Duration::ZERO);
    builder.add_scheduled_callback("a", schedule.clone(), || Ok(Source.bind(a_source, &a)?));
    builder.add_scheduled_callback("b", schedule, || Ok(Source.bind(b_source, &b)?));
    let recorder = logging::ExecutionRecorder::new(8);
    let graph = recorder.attach(builder.build().unwrap()).unwrap();
    let writer = Writer::default();
    let logger = LoggingScope::new(
        writer.clone(),
        vec![a_log.bind(&a).unwrap(), b_log.bind(&b).unwrap()],
        2,
    )
    .with_recording(recorder)
    .unwrap();
    let status = logger.status();
    {
        let graph = logger
            .attach_periodic(graph, Duration::from_nanos(10))
            .unwrap();
        let mut sim = simulation_executor::SimulationState::with_config(
            graph,
            simulation_executor::SimulationConfig {
                virtual_pool_threads: vec![2],
                node_executor_thread_count: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sim.step().unwrap().executed, [0, 1]);
        assert_eq!(sim.step().unwrap().executed, [2, 3]);
        assert_eq!(sim.step().unwrap().executed, [2, 3]);
    }
    logger.finish().unwrap();
    assert!(status.errors().is_empty());
    let state = writer.0.lock().unwrap();
    assert_eq!(
        state
            .messages
            .iter()
            .filter(|(channel, _, _)| channel == EXECUTION_LOG_CHANNEL)
            .count(),
        2
    );
    let descriptors: Vec<_> = state
        .artifacts
        .iter()
        .filter(|(name, _)| name == EXECUTION_LOG_DESCRIPTOR_ARTIFACT)
        .collect();
    assert_eq!(descriptors.len(), 1);
    let descriptor: ExecutionDescriptor = serde_json::from_slice(&descriptors[0].1).unwrap();
    assert_eq!(descriptor.logged_channels, ["a", "b"]);
    assert_eq!(
        descriptor
            .callbacks
            .iter()
            .map(|callback| callback.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[test]
fn scope_cleanup_after_unwind_flushes_tail_and_keeps_errors_observable() {
    let mut plan = ChannelPlan::<Wire>::new("data");
    let publisher = plan.publisher(1);
    let capture = CapturePlan::declare(&mut plan, 1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut publisher = bindings.take_publisher(&publisher).unwrap();
    publisher.publish(Wire(1)).unwrap();
    publisher.flush(at(0));
    let writer = Writer::default();
    writer.0.lock().unwrap().fail = true;
    let logger = LoggingScope::new(writer.clone(), vec![capture.bind(&bindings).unwrap()], 0);
    let status = logger.status();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _logger = logger;
            panic!("workload failure");
        }))
        .is_err()
    );
    assert!(
        status
            .errors()
            .iter()
            .any(|error| error.contains("scheduled writer failure"))
    );
    assert!(writer.0.lock().unwrap().flushes > 0);
}

#[test]
#[cfg_attr(miri, ignore = "uses wall-clock timing and scoped live workers")]
fn live_periodic_logger_flushes_while_workers_run() {
    let mut plan = ChannelPlan::new("data");
    let source = Source::declare(&mut plan).unwrap();
    let capture = CapturePlan::declare(&mut plan, 1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_scheduled_callback("source", CallbackSchedule::on_start(), || {
        Ok(Source.bind(source, &bindings)?)
    });
    let writer = Writer::default();
    let logger = LoggingScope::new(writer.clone(), vec![capture.bind(&bindings).unwrap()], 1);
    let graph = logger
        .attach_periodic(graph.build().unwrap(), Duration::from_millis(1))
        .unwrap();
    live_executor::LiveExecutor::new(2, graph)
        .unwrap()
        .run_with(|stop| {
            let start = std::time::Instant::now();
            while writer.0.lock().unwrap().messages.is_empty() {
                assert!(start.elapsed() < Duration::from_secs(5));
                std::thread::yield_now();
            }
            stop.request_stop();
        })
        .unwrap();
    logger.finish().unwrap();
    assert_eq!(writer.0.lock().unwrap().messages.len(), 1);
}
