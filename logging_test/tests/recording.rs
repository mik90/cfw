use logging::{CapturePlan, ExecutionRecord, ExecutionRecorder, InMemoryWriter, LogSession};
use simulation_executor::{SimulationConfig, SimulationState};
use std::time::Duration;
use task::{CallbackSchedule, ChannelPlan, GraphBuilder, GraphPlan, Output};
use task_macros::task_callback;

struct Counter(u64);
#[task_callback]
impl Counter {
    fn run(&mut self, mut value: Output<u64>) {
        *value = self.0;
        self.0 += 1;
        value.send();
    }
}

#[test]
fn selected_payloads_and_execution_snapshots_survive_simulation_shutdown() {
    for capture_payload in [true, false] {
        let writer = InMemoryWriter::new();
        let data = writer.logged_data();
        {
            let mut values = ChannelPlan::new("values");
            let declaration = Counter::declare(&mut values).unwrap();
            let capture = capture_payload.then(|| CapturePlan::declare(&mut values, 8));
            let storage = GraphPlan::new(values).allocate().unwrap();
            let values = storage.channels().build();
            let mut builder = GraphBuilder::with_storage(&storage);
            builder.add_scheduled_callback(
                "counter",
                CallbackSchedule::periodic(Duration::from_nanos(10))
                    .with_execution_duration(Duration::from_nanos(2)),
                || Ok(Counter(0).bind(declaration, &values)?),
            );
            let recorder = ExecutionRecorder::new(8);
            let graph = recorder.attach(builder.build().unwrap()).unwrap();
            let captures = capture
                .map(|c| c.bind(&values).unwrap())
                .into_iter()
                .collect();
            let mut session = LogSession::new(writer, captures)
                .with_recording(recorder)
                .unwrap();
            let mut simulation = SimulationState::with_config(
                graph,
                SimulationConfig {
                    poll_external_events: false,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut executions = 0;
            for _ in 0..16 {
                executions += simulation.step().unwrap().executed.len();
                session.flush().unwrap();
                if executions == 4 {
                    break;
                }
            }
            assert_eq!(executions, 4);
            session.finish().unwrap();
        }
        let data = data.lock().unwrap();
        assert_eq!(data.artifacts().len(), 2);
        let payloads: Vec<_> = data
            .messages()
            .iter()
            .filter(|m| m.channel() == "values")
            .collect();
        assert_eq!(payloads.len(), if capture_payload { 4 } else { 0 });
        let records: Vec<ExecutionRecord> = data
            .messages()
            .iter()
            .filter(|m| m.channel() == task::recording::EXECUTION_LOG_CHANNEL)
            .map(|m| serde_json::from_slice(m.body()).unwrap())
            .collect();
        assert_eq!(records.len(), 4);
        for (index, record) in records.iter().enumerate() {
            assert_eq!(
                record.execution_time.to_nanoseconds(),
                10 + index as i64 * 12
            );
            assert_eq!(record.outputs.len(), 1);
            assert_eq!(record.outcome, task::recording::Outcome::Committed);
            if capture_payload {
                assert_eq!(
                    serde_json::from_slice::<u64>(payloads[index].body()).unwrap(),
                    index as u64
                );
                assert_eq!(payloads[index].header().published_at, record.execution_time);
            }
        }
    }
}
