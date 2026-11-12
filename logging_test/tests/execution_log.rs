use logging::{ExecutionLogOptions, ExecutionLogPlan, InMemoryWriter, LogSession};
use simulation_executor::{SimulationConfig, SimulationState};
use std::time::Duration;
use task::{
    Callback, ChannelPlan, Context, LoanError, OutputDefault, Publisher,
    execution_log_schema::{
        Direction, EXECUTION_LOG_CHANNEL, EndpointDescriptor, ExecutionDescriptor,
        ExecutionLogMode, ExecutionLogModes, ExecutionRecord, LoggedMessage, Outcome, Transport,
    },
    testing::FakeCallback,
    time::FrameworkTime,
};
use task_graph::{
    CallbackSchedule, GraphBuilder, GraphPlan, TaskRegistration, ThreadPoolConfigList,
};
use task_macros::task_callback;

struct Counter;
#[task_callback]
impl Counter {
    fn run(&mut self, mut output: OutputDefault<u64>) {
        *output = 1;
        output.send();
    }
}

struct Run {
    records: Vec<ExecutionRecord>,
    descriptor: ExecutionDescriptor,
}

fn start_plan(names: &[&str]) -> GraphPlan {
    let mut plan = GraphPlan::default();
    for name in names {
        TaskRegistration::new(
            *name,
            Counter,
            CallbackSchedule::on_start().with_execution_duration(Duration::from_nanos(1)),
        )
        .register(&mut plan)
        .unwrap();
    }
    plan
}

fn run(plan: GraphPlan, options: ExecutionLogOptions) -> Run {
    let storage = plan.allocate().unwrap();
    let graph = storage.build().unwrap();
    let execution_log = ExecutionLogPlan::for_graph(&graph, options).unwrap();
    let log_storage = execution_log.allocate().unwrap();
    let (graph, metadata, captures) = log_storage.attach(graph).unwrap();

    let writer = InMemoryWriter::new();
    let data = writer.logged_data();
    let session = LogSession::new(writer, captures)
        .with_execution_log(metadata.clone())
        .unwrap();
    let mut simulation = SimulationState::with_config(
        graph,
        SimulationConfig {
            start_time: FrameworkTime::from_nanos(7),
            virtual_pool_threads: ThreadPoolConfigList::try_from([4]).unwrap(),
            ..Default::default()
        },
    )
    .unwrap();
    simulation.step().unwrap();
    session.finish().unwrap();

    let records = data
        .lock()
        .unwrap()
        .messages()
        .iter()
        .filter(|message| message.channel() == EXECUTION_LOG_CHANNEL)
        .map(|message| serde_json::from_slice(message.body()).unwrap())
        .collect();
    Run {
        records,
        descriptor: metadata.descriptor,
    }
}

fn run_builder<'build, 'graph_storage>(
    builder: GraphBuilder<'build, 'graph_storage>,
    options: ExecutionLogOptions,
) -> (
    Result<simulation_executor::StepResult, simulation_executor::StepError>,
    Vec<ExecutionRecord>,
) {
    let graph = builder.build().unwrap();
    let execution_log = ExecutionLogPlan::for_graph(&graph, options).unwrap();
    let log_storage = execution_log.allocate().unwrap();
    let (graph, _metadata, mut captures) = log_storage.attach(graph).unwrap();
    let mut simulation = SimulationState::with_config(
        graph,
        SimulationConfig {
            poll_external_events: false,
            ..Default::default()
        },
    )
    .unwrap();
    let result = simulation.step();
    let records = captures[0]
        .drain_to_vec()
        .unwrap()
        .iter()
        .map(|(_, body)| serde_json::from_slice(body).unwrap())
        .collect();
    (result, records)
}

fn duration_only() -> ExecutionLogOptions {
    ExecutionLogOptions::new(8, 2)
        .with_modes(ExecutionLogModes::new(ExecutionLogMode::DurationOnly))
}

#[test]
fn full_mode_records_each_callback() {
    let run = run(start_plan(&["a", "b"]), ExecutionLogOptions::new(8, 2));

    assert_eq!(run.records.len(), 2);
    assert_eq!(run.descriptor.callbacks.len(), 2);
    for record in &run.records {
        assert_eq!(record.execution_time, FrameworkTime::from_nanos(7));
        assert_eq!(record.outcome, Outcome::Committed);
        assert_eq!(record.outputs.len(), 1);
        assert_eq!(
            run.descriptor.callbacks[record.callback_id.index()].mode,
            ExecutionLogMode::Full
        );
    }
}

#[test]
fn modes_select_publishers_and_record_detail() {
    for default in [
        ExecutionLogMode::Off,
        ExecutionLogMode::DurationOnly,
        ExecutionLogMode::Full,
    ] {
        let modes = ExecutionLogModes::new(default)
            .with_callback("off", ExecutionLogMode::Off)
            .with_callback("duration", ExecutionLogMode::DurationOnly)
            .with_callback("full", ExecutionLogMode::Full);
        let run = run(
            start_plan(&["default", "off", "duration", "full"]),
            ExecutionLogOptions::new(8, 2).with_modes(modes),
        );

        assert_eq!(
            run.records.len(),
            if default == ExecutionLogMode::Off {
                2
            } else {
                3
            }
        );
        for record in &run.records {
            let descriptor = &run.descriptor.callbacks[record.callback_id.index()];
            assert_ne!(descriptor.mode, ExecutionLogMode::Off);
            assert_eq!(
                record.outputs.is_empty(),
                descriptor.mode == ExecutionLogMode::DurationOnly
            );
        }
    }
}

#[test]
fn plan_rejects_invalid_capacities_and_callback_names() {
    let storage = start_plan(&["counter"]).allocate().unwrap();
    let graph = storage.build().unwrap();

    assert!(ExecutionLogPlan::for_graph(&graph, ExecutionLogOptions::new(0, 1)).is_err());
    assert!(ExecutionLogPlan::for_graph(&graph, ExecutionLogOptions::new(1, 0)).is_err());

    let modes = ExecutionLogModes::new(ExecutionLogMode::Full)
        .with_callback("missing", ExecutionLogMode::Off);
    assert!(
        ExecutionLogPlan::for_graph(&graph, ExecutionLogOptions::new(1, 1).with_modes(modes))
            .is_err()
    );
}

#[test]
fn body_panic_records_body_panicked() {
    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback(
        "worker",
        CallbackSchedule::on_start().with_execution_duration(Duration::ZERO),
        || {
            Ok(FakeCallback {
                run_hook: Box::new(|_| -> Result<(), LoanError> { panic!("body failed") }),
                ..Default::default()
            })
        },
    );

    let (result, records) = run_builder(builder, duration_only());

    assert!(result.is_err());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, Outcome::BodyPanicked);
}

#[test]
fn body_error_records_body_error() {
    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback(
        "worker",
        CallbackSchedule::on_start().with_execution_duration(Duration::ZERO),
        || {
            Ok(FakeCallback {
                run_hook: Box::new(|_| Err(LoanError::LoanCapacityReached)),
                ..Default::default()
            })
        },
    );

    let (result, records) = run_builder(builder, duration_only());

    assert!(result.is_err());
    assert_eq!(records.len(), 1);
    assert!(matches!(records[0].outcome, Outcome::BodyError(_)));
}

struct CommitPanic<'a> {
    output: Publisher<'a, u64>,
}

impl Callback for CommitPanic<'_> {
    fn outputs_are_local(&self) -> bool {
        true
    }
    fn recording_endpoints(&self) -> Option<Vec<EndpointDescriptor>> {
        Some(vec![EndpointDescriptor {
            ordinal: 0,
            channel: "output".into(),
            direction: Direction::Published,
            transport: Transport::Native,
            payload_type: std::any::type_name::<u64>().into(),
            publisher_index: Some(self.output.publisher_index()),
        }])
    }
    fn run(&mut self, _: &Context) -> Result<(), LoanError> {
        self.output.publish(7)
    }
    fn visit_pending_messages(&self, visit: &mut dyn FnMut(LoggedMessage)) {
        self.output.visit_pending_headers(|header| {
            visit(LoggedMessage { ordinal: 0, header });
        });
    }
    fn flush_outputs(&mut self, _: FrameworkTime) {
        panic!("commit failed");
    }
    fn discard_outputs(&mut self) {
        self.output.discard_pending();
    }
}

#[test]
fn commit_panic_records_attempts() {
    let mut outputs = ChannelPlan::new("output");
    let key = outputs.publisher(1);
    let storage = outputs.allocate().unwrap();
    let bindings = storage.bind();
    let mut builder = GraphBuilder::with_channel_names([storage.name()]);
    builder.add_scheduled_callback(
        "worker",
        CallbackSchedule::on_start().with_execution_duration(Duration::ZERO),
        || {
            Ok(CommitPanic {
                output: bindings.take_publisher(&key)?,
            })
        },
    );

    let (result, records) = run_builder(builder, ExecutionLogOptions::new(8, 2));

    assert!(result.is_err());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, Outcome::CommitPanicked);
    assert_eq!(records[0].outputs.len(), 1);
}
