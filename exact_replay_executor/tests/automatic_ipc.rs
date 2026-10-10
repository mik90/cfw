#![cfg(feature = "iceoryx2")]
use exact_replay_executor::{AutomaticExactReplayPlan, ExactReplayExecutor, ReplayLog};
use logging::{AutomaticCapturePlan, CaptureOptions, ExecutionRecorder, LogSession};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use task::{
    CallbackSchedule,
    automatic::{NamedPlan, TaskRegistration},
    iox2::{Iox2Event, Iox2Output, Iox2SpanInput},
    time::FrameworkTime,
};
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
    fn run(&self, mut output: Iox2Output<u64>) {
        *output = 42;
        output.send();
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
fn source(plan: &mut NamedPlan, data: &str) -> task::automatic::RegisteredTask {
    let mut task = TaskRegistration::new("source", Source, CallbackSchedule::default());
    task.output_channel("output", data);
    task.register(plan).unwrap()
}
fn sink(
    plan: &mut NamedPlan,
    data: &str,
    output: &str,
    event: &str,
) -> task::automatic::RegisteredTask {
    let mut task = TaskRegistration::new("sink", Sink, CallbackSchedule::default());
    task.input_channel("event", event)
        .input_channel("input", data)
        .output_channel("output", output);
    task.register(plan).unwrap()
}
#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn automatic_ipc_hydration_is_port_local_and_preserves_event_ordinals() {
    let prefix = format!("cfw_auto_exact_{}", std::process::id());
    let data = format!("{prefix}_data");
    let output = format!("{prefix}_output");
    let event = format!("{prefix}_event");
    let bytes = Bytes::default();
    {
        let mut plan = NamedPlan::default();
        let source = source(&mut plan, &data);
        let sink = sink(&mut plan, &data, &output, &event);
        let capture = AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(4)).unwrap();
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        let mut graph = storage.graph_builder();
        source.add_to_graph(&mut graph, &bindings);
        sink.add_to_graph(&mut graph, &bindings);
        let recorder = ExecutionRecorder::new(4);
        let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
        let session = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            capture.bind(&bindings).unwrap(),
        )
        .with_recording(recorder)
        .unwrap();
        graph.step(FrameworkTime::from_nanoseconds(0)).unwrap();
        graph.step(FrameworkTime::from_nanoseconds(10)).unwrap();
        session.finish().unwrap();
    }
    let log = ReplayLog::from_reader(
        &logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap(),
    )
    .unwrap();
    let mut plan = NamedPlan::default();
    let sink = sink(&mut plan, &data, &output, &event);
    let source = source(&mut plan, &data);
    let replay = AutomaticExactReplayPlan::declare(&mut plan, &log).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let replay = replay.bind(&bindings).unwrap();
    let mut graph = storage.graph_builder();
    sink.add_to_graph(&mut graph, &bindings);
    source.add_to_graph(&mut graph, &bindings);
    assert!(
        ExactReplayExecutor::new(graph.build().unwrap(), log, replay)
            .unwrap()
            .run()
            .unwrap()
            .is_exact()
    );
}
