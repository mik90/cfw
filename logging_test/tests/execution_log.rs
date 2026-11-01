use logging::{ExecutionLogPlan, InMemoryWriter, LogSession};
use std::time::Duration;
use task::{execution_log_schema::ExecutionRecord, time::FrameworkTime};
use task_graph::{CallbackSchedule, GraphPlan, TaskRegistration};
use task_macros::task_callback;

struct Counter;
#[task_callback]
impl Counter {
    fn run(&mut self) {}
}

#[test]
fn execution_log_publishers_feed_the_log_session() {
    let mut plan = GraphPlan::default();
    TaskRegistration::new(
        "counter",
        Counter,
        CallbackSchedule::on_start().with_execution_duration(Duration::from_nanos(1)),
    )
    .register(&mut plan)
    .unwrap();
    let execution_log = ExecutionLogPlan::declare(
        &mut plan,
        task::execution_log_schema::RecordingOptions::default(),
        8,
    )
    .unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let (graph, metadata, captures) = execution_log
        .attach(storage.build_with(&bindings).unwrap(), &bindings)
        .unwrap();
    let writer = InMemoryWriter::new();
    let data = writer.logged_data();
    let session = LogSession::new(writer, captures)
        .with_execution_log(metadata)
        .unwrap();
    let mut simulation = simulation_executor::SimulationState::with_config(
        graph,
        simulation_executor::SimulationConfig {
            start_time: FrameworkTime::from_nanoseconds(7),
            ..Default::default()
        },
    )
    .unwrap();
    simulation.step().unwrap();
    session.finish().unwrap();
    let data = data.lock().unwrap();
    let records: Vec<ExecutionRecord> = data
        .messages()
        .iter()
        .filter(|message| message.channel() == task::execution_log_schema::EXECUTION_LOG_CHANNEL)
        .map(|message| serde_json::from_slice(message.body()).unwrap())
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].execution_time,
        FrameworkTime::from_nanoseconds(7)
    );
}

#[test]
fn execution_log_plan_applies_per_callback_modes() {
    use task::execution_log_schema::{RecordingMode, RecordingOptions};
    for default in [
        RecordingMode::Off,
        RecordingMode::DurationOnly,
        RecordingMode::Full,
    ] {
        let mut plan = GraphPlan::default();
        for name in ["default", "off", "duration", "full"] {
            TaskRegistration::new(
                name,
                Counter,
                CallbackSchedule::on_start().with_execution_duration(Duration::from_nanos(1)),
            )
            .register(&mut plan)
            .unwrap();
        }
        let options = RecordingOptions::new(default)
            .with_callback("off", RecordingMode::Off)
            .with_callback("duration", RecordingMode::DurationOnly)
            .with_callback("full", RecordingMode::Full);
        let execution_log = ExecutionLogPlan::declare(&mut plan, options, 8).unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let (graph, metadata, mut captures) = execution_log
            .attach(storage.build_with(&bindings).unwrap(), &bindings)
            .unwrap();
        let mut simulation = simulation_executor::SimulationState::with_config(
            graph,
            simulation_executor::SimulationConfig {
                virtual_pool_threads: vec![4],
                start_time: FrameworkTime::from_nanoseconds(7),
                ..Default::default()
            },
        )
        .unwrap();
        simulation.step().unwrap();
        let rows = captures[0].drain_to_vec().unwrap();
        let records: Vec<ExecutionRecord> = rows
            .iter()
            .map(|(_, body)| serde_json::from_slice(body).unwrap())
            .collect();
        assert_eq!(
            records.len(),
            if default == RecordingMode::Off { 2 } else { 3 }
        );
        for record in records {
            assert_eq!(record.execution_time, FrameworkTime::from_nanoseconds(7));
            assert!(
                metadata.descriptor.callbacks[record.callback_id.index()].recording_mode
                    != RecordingMode::Off
            );
        }
    }
}
