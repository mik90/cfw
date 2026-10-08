#![cfg(feature = "iceoryx2")]

use iceoryx2::prelude::EventId;
use simulation_executor::StepError;
use std::sync::{Arc, Mutex};
use task::iox2::{Iox2ChannelPlan, Iox2Event, Iox2OptionalInput, Iox2Output, Iox2Runtime};
use task::{ChannelPlan, Context, GraphBuilder, GraphPlan, RequiredInput};
use task_macros::task_callback;
use testing::BoundUnitTestExecutorBuilder as UnitTestExecutorBuilder;

type Observations = Arc<Mutex<Vec<(i64, u64, Vec<(usize, u64)>)>>>;
struct Observe {
    observations: Observations,
}
#[task_callback]
impl Observe {
    fn run(
        &self,
        input: Iox2OptionalInput<u64>,
        event: Iox2Event,
        #[trigger(false)] gate: RequiredInput<u64>,
        mut output: Iox2Output<u64>,
        context: &Context,
    ) {
        self.observations.lock().unwrap().push((
            context.now().to_nanoseconds(),
            *input.value().unwrap(),
            event
                .records()
                .map(|(id, count)| (id.as_value(), count))
                .collect(),
        ));
        *output = input.value().unwrap() + *gate;
        output.send();
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn counted_events_survive_native_gating_and_capture_outlives_executor() {
    let runtime = Iox2Runtime::new().unwrap();
    let channel = format!("unit_fixture_in_{}", std::process::id());
    let mut input = Iox2ChannelPlan::new(&channel, &runtime);
    let mut output =
        Iox2ChannelPlan::new(format!("unit_fixture_out_{}", std::process::id()), &runtime);
    let mut gate = ChannelPlan::new("gate");
    let declaration = ObserveDeclaration::from_keys(
        input.subscriber(1),
        input.events(4),
        gate.subscriber(1),
        output.publisher(1),
    );
    let input_key = input.publisher(1);
    let gate_key = gate.publisher(1);
    let capture_key = output.subscriber(2);
    let storage = GraphPlan::new((input, (output, gate))).allocate().unwrap();
    let input = storage.channels().0.build().unwrap();
    let output = storage.channels().1.0.build().unwrap();
    let gate = storage.channels().1.1.build();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_scheduled_callback(
        "observe",
        task::CallbackSchedule::default().with_execution_duration(std::time::Duration::ZERO),
        || {
            Ok(Observe {
                observations: observations.clone(),
            }
            .bind(declaration, &input, &input, &gate, &output)?)
        },
    );
    let builder = UnitTestExecutorBuilder::new(graph.build().unwrap());
    let sender = builder.add_iox2_test_publisher(input.take_publisher(&input_key).unwrap());
    let events = builder.add_iox2_test_notifier(&channel);
    let mut gate_sender = builder.add_test_publisher(gate.take_publisher(&gate_key).unwrap());
    let capture = builder.add_iox2_test_subscriber(output.take_subscriber(&capture_key).unwrap());
    let mut executor = builder.build();
    sender.send(77);
    events.notify(EventId::new(9), 5);
    assert!(executor.step().executed.is_empty());
    assert!(observations.lock().unwrap().is_empty());
    gate_sender.send(11);
    assert_eq!(executor.step().executed, [0]);
    assert!(executor.step().executed.is_empty());
    assert_eq!(*observations.lock().unwrap(), [(0, 77, vec![(9, 5)])]);
    let messages = capture.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].message, 88);
    assert_eq!(messages[0].header.published_at.to_nanoseconds(), 0);
    assert!(capture.messages().is_empty());
    sender.send(78);
    events.notify(EventId::new(4), 2);
    executor.step();
    drop(executor);
    assert_eq!(capture.messages()[0].message, 89);
    assert!(std::panic::catch_unwind(|| events.notify(EventId::new(0), 1)).is_err());
}

#[test]
fn unknown_event_channel_returns_error_and_poisons_session() {
    let builder = UnitTestExecutorBuilder::new(GraphBuilder::new().build().unwrap());
    let events = builder.add_iox2_test_notifier("missing");
    let mut executor = builder.build();
    events.notify(EventId::new(0), 1);
    assert!(matches!(
        executor.try_step(),
        Err(StepError::UnknownEventChannel(_))
    ));
    assert!(matches!(executor.try_step(), Err(StepError::Poisoned)));
}
