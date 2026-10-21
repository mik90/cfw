#![cfg(feature = "iceoryx2")]
use exact_replay_executor::{ExactReplayExecutor, ExactReplayPlan, ReplayLog};
use logging::{CaptureOptions, ExecutionRecorder, GraphCapturePlan, LogSession};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use task::{
    iox2::{Iox2Event, Iox2NotifyOutput, Iox2Output, Iox2SpanInput},
    time::FrameworkTime,
};
use task_graph::{CallbackSchedule, GraphPlan, TaskRegistration};
use task_macros::task_callback;
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
struct Source;
#[task_callback]
impl Source {
    fn run(&self, mut output: Iox2Output<u64>, first: Iox2NotifyOutput, second: Iox2NotifyOutput) {
        *output = 42;
        output.send();
        first.send();
        second.send();
    }
}
struct Sink;
#[task_callback]
impl Sink {
    fn run(&self, event: Iox2Event, input: Iox2SpanInput<u64>, mut output: Iox2Output<u64>) {
        let _ = event;
        *output = input.inputs().map(|message| message.message).sum();
        output.send();
    }
}
fn source(plan: &mut GraphPlan, data: &str, event: &str) {
    let mut task = TaskRegistration::new("source", Source, CallbackSchedule::default());
    task.output_channel("output", data);
    task.output_channel("first", event)
        .output_channel("second", event);
    task.register(plan).unwrap()
}
fn sink(plan: &mut GraphPlan, data: &str, output: &str, event: &str) {
    let mut task = TaskRegistration::new("sink", Sink, CallbackSchedule::default());
    task.input_channel("event", event)
        .input_channel("input", data)
        .output_channel("output", output);
    task.register(plan).unwrap()
}
#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn automatic_ipc_hydration_is_endpoint_local_and_preserves_event_ordinals() {
    let prefix = format!("cfw_auto_exact_{}", std::process::id());
    let data = format!("{prefix}_data");
    let output = format!("{prefix}_output");
    let event = format!("{prefix}_event");
    let bytes = Bytes::default();
    {
        let mut plan = GraphPlan::default();
        source(&mut plan, &data, &event);
        sink(&mut plan, &data, &output, &event);
        let capture = GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(16)).unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let recorder = ExecutionRecorder::new(16);
        let mut graph = storage
            .build_with(&bindings)
            .unwrap()
            .with_recording(&recorder)
            .unwrap();
        graph.configure_schedules(|_, schedule| {
            *schedule = task_graph::CallbackSchedule {
                run_on_start: true,
                ..task_graph::CallbackSchedule::periodic(std::time::Duration::from_nanos(10))
                    .with_execution_duration(std::time::Duration::ZERO)
            };
        });
        let mut graph = testing::UnitTestExecutor::new_with_graph(graph, Default::default());
        let session = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            capture.bind(&bindings).unwrap(),
        )
        .with_recording(recorder)
        .unwrap();
        graph
            .run_until(FrameworkTime::from_nanoseconds(11), 32)
            .unwrap();
        session.finish().unwrap();
    }
    let log = ReplayLog::from_reader(
        &logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap(),
    )
    .unwrap();
    let mut plan = GraphPlan::default();
    sink(&mut plan, &data, &output, &event);
    source(&mut plan, &data, &event);
    let replay = ExactReplayPlan::declare(&mut plan, &log).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let replay = replay.bind(&bindings).unwrap();
    assert!(
        ExactReplayExecutor::new(storage.build_with(&bindings).unwrap(), log, replay)
            .unwrap()
            .run()
            .unwrap()
            .is_exact()
    );
}
