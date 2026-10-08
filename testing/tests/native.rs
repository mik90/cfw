use simulation_executor::StepError;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use task::{
    CallbackSchedule, ChannelPlan, GraphBuilder, GraphPlan, LoanError, Publisher, RequiredInput,
    time::FrameworkTime,
};
use task_macros::task_callback;
use testing::{UnitTestExecutorBuilder, UnitTestExecutorConfig};

fn at(n: i64) -> FrameworkTime {
    FrameworkTime::from_nanoseconds(n)
}

struct Double;
#[task_callback]
impl Double {
    fn run(&self, input: RequiredInput<u64>, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(*input * 2)
    }
}

#[test]
fn fixture_round_trip_timestamps_idle_restart_and_executor_first_drop() {
    let mut input = ChannelPlan::new("input");
    let mut output = ChannelPlan::new("output");
    let declaration = Double::declare(&mut input, &mut output).unwrap();
    let sender = input.publisher(1);
    let capture = output.subscriber(4);
    let storage = GraphPlan::new((input, output)).allocate().unwrap();
    let (input, output) = storage.channels();
    let input = input.build();
    let output = output.build();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_callback("double", || {
        Ok(Double.bind(declaration, &input, &output)?)
    });
    let builder = UnitTestExecutorBuilder::with_config(
        graph.build().unwrap(),
        UnitTestExecutorConfig {
            start_time: at(100),
            ..Default::default()
        },
    );
    let mut sender = builder.add_test_publisher(input.take_publisher(&sender).unwrap());
    let mut capture = builder.add_test_subscriber(output.take_subscriber(&capture).unwrap());
    let mut executor = builder.build();
    drop((input, output));
    assert!(executor.step().idle);
    sender.send(21);
    let step = executor.step();
    assert_eq!(step.executed, [0]);
    assert_eq!((step.before, step.after), (at(100), at(100)));
    assert_eq!(executor.step_count().0, 2);
    sender.send(22);
    executor.step();
    drop(executor);
    let retained = capture.take_messages();
    assert!(capture.take_messages().is_empty());
    drop((capture, sender));
    assert_eq!(
        retained.iter().map(|m| m.message).collect::<Vec<_>>(),
        [42, 44]
    );
    assert!(retained.iter().all(|m| m.header.published_at == at(100)));
}

struct Tick;
#[task_callback]
impl Tick {
    fn run(&self) {}
}

#[test]
fn fixture_clock_tracks_virtual_time_and_custom_configuration() {
    let mut plan = ChannelPlan::<u64>::new("fixture_only");
    let publisher = plan.publisher(1);
    let subscriber = plan.subscriber(2);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_scheduled_callback(
        "tick",
        CallbackSchedule::periodic(Duration::from_nanos(10)).in_pool(1),
        || Ok(Tick.bind(Tick::declare()?)?),
    );
    let builder = UnitTestExecutorBuilder::with_config(
        graph.build().unwrap(),
        UnitTestExecutorConfig {
            start_time: at(100),
            virtual_pool_threads: vec![1, 1],
            node_executor_thread_count: 2,
            ..Default::default()
        },
    );
    let mut sender = builder.add_test_publisher(bindings.take_publisher(&publisher).unwrap());
    let mut capture = builder.add_test_subscriber(bindings.take_subscriber(&subscriber).unwrap());
    let mut executor = builder.build();
    sender.send(1);
    let step = executor.step();
    assert_eq!((step.before, step.after), (at(100), at(110)));
    assert_eq!(executor.current_time(), at(110));
    sender.send(2);
    let messages = capture.take_messages();
    assert_eq!(messages[0].header.published_at, at(100));
    assert_eq!(messages[1].header.published_at, at(110));
}

#[test]
fn capture_reports_overflow_and_indexes_each_batch_from_zero() {
    let mut plan = ChannelPlan::<u64>::new("fixture_only");
    let publisher = plan.publisher(1);
    let subscriber = plan.subscriber(1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let builder =
        UnitTestExecutorBuilder::new(GraphBuilder::with_storage(&storage).build().unwrap());
    let mut sender = builder.add_test_publisher(bindings.take_publisher(&publisher).unwrap());
    let mut capture = builder.add_test_subscriber(bindings.take_subscriber(&subscriber).unwrap());
    let executor = builder.build();
    sender.send(1);
    sender.send(2);
    let (count, dropped) = capture.try_messages(|index, message| {
        assert_eq!((index, message.message), (0, 2));
    });
    assert_eq!((count, dropped.writer, dropped.reader), (1, 1, 0));
    drop(executor);
    sender.send(3);
    assert_eq!(
        capture
            .try_messages(|index, m| assert_eq!((index, m.message), (0, 3)))
            .0,
        1
    );
    assert_eq!(capture.try_messages(|_, _| panic!("empty batch")).0, 0);
}

struct Tracked(Arc<AtomicUsize>);
impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn retained_payload_drops_exactly_once_after_all_endpoints_and_executor() {
    let mut plan = ChannelPlan::new("tracked");
    let publisher = plan.publisher(1);
    let subscriber = plan.subscriber(1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let builder =
        UnitTestExecutorBuilder::new(GraphBuilder::with_storage(&storage).build().unwrap());
    let mut sender = builder.add_test_publisher(bindings.take_publisher(&publisher).unwrap());
    let mut capture = builder.add_test_subscriber(bindings.take_subscriber(&subscriber).unwrap());
    let executor = builder.build();
    let drops = Arc::new(AtomicUsize::new(0));
    sender.send(Tracked(drops.clone()));
    let retained = capture.take_messages();
    sender.send(Tracked(drops.clone()));
    drop((bindings, executor, sender, capture));
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(retained);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

struct Fails;
#[task_callback]
impl Fails {
    fn run(&self, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(7)?;
        Err(LoanError::Transport("expected failure".into()))
    }
}
#[test]
fn failed_step_cancels_capture_and_poisons_executor() {
    let mut plan = ChannelPlan::new("failure");
    let declaration = Fails::declare(&mut plan).unwrap();
    let capture = plan.subscriber(1);
    let sender = plan.publisher(1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_scheduled_callback(
        "fails",
        CallbackSchedule::periodic(Duration::from_nanos(10)),
        || Ok(Fails.bind(declaration, &bindings)?),
    );
    let builder = UnitTestExecutorBuilder::new(graph.build().unwrap());
    let mut capture = builder.add_test_subscriber(bindings.take_subscriber(&capture).unwrap());
    let mut sender = builder.add_test_publisher(bindings.take_publisher(&sender).unwrap());
    let mut executor = builder.build();
    assert_eq!(executor.step().after, at(10));
    assert!(matches!(
        executor.try_step(),
        Err(StepError::Callback { .. })
    ));
    assert!(capture.take_messages().is_empty());
    sender.send(9);
    let messages = capture.take_messages();
    assert_eq!(messages[0].message, 9);
    assert_eq!(messages[0].header.published_at, at(10));
    assert!(matches!(executor.try_step(), Err(StepError::Poisoned)));
}

#[test]
fn invalid_configuration_is_reported_by_try_build() {
    let graph = GraphBuilder::new().build().unwrap();
    let builder = UnitTestExecutorBuilder::with_config(
        graph,
        UnitTestExecutorConfig {
            node_executor_thread_count: 0,
            ..Default::default()
        },
    );
    assert!(matches!(
        builder.try_build(),
        Err(StepError::InvalidConfig(_))
    ));
}
