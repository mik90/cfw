#![cfg(feature = "log_simulation")]
use logging::{AutomaticCapturePlan, AutomaticReplayPlan, CaptureOptions, ReplayOptions};
use task::{
    CallbackSchedule, InputSpan, LoanError, Publisher,
    automatic::{NamedPlan, TaskRegistration},
    message::MessageHeader,
    time::FrameworkTime,
};
use task_macros::task_callback;
struct Sum;
#[task_callback]
impl Sum {
    fn run(&self, input: InputSpan<u64>, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(input.inputs().map(|message| message.message).sum())
    }
}
#[test]
fn automatic_sources_drive_log_simulation_through_overridden_input_names() {
    let mut plan = NamedPlan::default();
    let mut registration = TaskRegistration::new(
        "sum",
        Sum,
        CallbackSchedule::default().with_execution_duration(std::time::Duration::ZERO),
    );
    registration.input_channel("input", "wire");
    let task = registration.register(&mut plan).unwrap();
    let header = MessageHeader {
        published_at: FrameworkTime::from_nanoseconds(13),
        publisher_index: 7,
        batch_index: 2,
    };
    let reader = logging::SortedLogStreamReader::from_entries(
        vec![logging::OwnedLogEntry {
            channel_name: "wire".into(),
            header,
            serialized_body: b"17".to_vec(),
        }],
        Default::default(),
    )
    .unwrap();
    let replay = AutomaticReplayPlan::from_log(&mut plan, &reader, &ReplayOptions::new(1)).unwrap();
    let captures =
        AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(1).exclude("wire")).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let sources = replay.bind(&bindings).unwrap();
    let mut captures = captures.bind(&bindings).unwrap();
    let mut graph = storage.graph_builder();
    task.add_to_graph(&mut graph, &bindings);
    let mut simulation =
        simulation_executor::LogSimulation::new(graph.build().unwrap(), reader, sources).unwrap();
    simulation.run_until_idle(8).unwrap();
    assert!(simulation.input_exhausted());
    let messages = captures[0].drain_to_vec().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0.published_at, header.published_at);
    assert_eq!(messages[0].1, b"17");
}
