use logging::{ExecutionLogOptions, ExecutionLogPlan, InMemoryWriter, LogSession};
use simulation_executor::{SimulationConfig, SimulationState};
use std::time::Duration;
use task::{
    Output,
    execution_log_schema::{
        EXECUTION_LOG_CHANNEL, ExecutionDescriptor, ExecutionLogMode, ExecutionLogModes,
        ExecutionRecord, Outcome,
    },
    time::FrameworkTime,
};
use task_graph::{CallbackSchedule, GraphPlan, TaskRegistration};
use task_macros::task_callback;

struct Counter;
#[task_callback]
impl Counter {
    fn run(&mut self, mut output: Output<u64>) {
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
            start_time: FrameworkTime::from_nanoseconds(7),
            virtual_pool_threads: vec![4],
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

#[test]
fn full_mode_records_each_callback() {
    let run = run(start_plan(&["a", "b"]), ExecutionLogOptions::new(8, 2));

    assert_eq!(run.records.len(), 2);
    assert_eq!(run.descriptor.callbacks.len(), 2);
    for record in &run.records {
        assert_eq!(record.execution_time, FrameworkTime::from_nanoseconds(7));
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
