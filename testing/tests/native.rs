use simulation_executor::StepError;
use std::time::Duration;
use task::{ChannelPlan, LoanError, Output, RequiredInput, time::FrameworkTime};
use task_graph::{CallbackSchedule, GraphBuilder, PoolIndex, ThreadPoolConfigList};
use task_macros::task_callback;
use testing::{UnitTestExecutor, UnitTestExecutorConfig};

fn at(n: i64) -> FrameworkTime {
    FrameworkTime::from_nanos(n)
}

struct Double;
#[task_callback]
impl Double {
    fn run(
        &self,
        #[trigger(true)] input: RequiredInput<u64>,
        output: Output<u64>,
    ) -> Result<(), LoanError> {
        output.write(*input * 2).send();
        Ok(())
    }
}

#[test]
fn fixture_round_trip_timestamps_idle_restart_and_executor_first_drop() {
    let mut input = ChannelPlan::new("input");
    let mut output = ChannelPlan::new("output");
    let declaration = Double::declare(&mut input, &mut output).unwrap();
    let sender = input.publisher(1);
    let capture = output.subscriber(4);
    let storage = (input.allocate().unwrap(), output.allocate().unwrap());
    let (input, output) = &storage;
    let input = input.bind();
    let output = output.bind();
    let mut graph = GraphBuilder::with_channel_names([storage.0.name(), storage.1.name()]);
    graph.add_scheduled_callback(
        "double",
        CallbackSchedule::input_triggered().with_execution_duration(Duration::ZERO),
        || Ok(Double.bind(declaration, &input, &output)?),
    );
    let mut executor = UnitTestExecutor::new_with_graph(
        graph.build().unwrap(),
        UnitTestExecutorConfig {
            start_time: at(100),
            ..Default::default()
        },
    );
    let mut sender = executor.add_test_publisher(input.take_publisher(&sender).unwrap());
    let capture = executor.add_test_subscriber(output.take_subscriber(&capture).unwrap());
    drop((input, output));
    assert!(executor.step().idle);
    sender.send(21);
    let step = executor.step();
    assert_eq!(step.executed, [0]);
    assert_eq!((step.before, step.after), (at(100), at(100)));
    assert_eq!(executor.step_count().0, 2);
    sender.send_at(at(110), 22);
    executor.run_until_idle(8).unwrap();
    assert!(sender.try_send_at(at(109), 23).is_err());
    drop(executor);
    assert!(sender.try_send_at(at(200), 23).is_err());
    let mut values = Vec::new();
    let mut stamps = Vec::new();
    assert_eq!(
        capture.consume(|index, message| {
            assert_eq!(index, values.len());
            values.push(message.message);
            stamps.push(message.header.published_at);
        }),
        2
    );
    assert_eq!(capture.message_count(), 0);
    drop((capture, sender));
    assert_eq!(values, [42, 44]);
    assert_eq!(stamps, [at(100), at(110)]);
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
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind();
    let mut graph = GraphBuilder::with_channel_names([storage.name()]);
    graph.add_scheduled_callback(
        "tick",
        CallbackSchedule::periodic(Duration::from_nanos(10))
            .in_pool(PoolIndex::new(1))
            .with_execution_duration(Duration::ZERO),
        || Ok(Tick.bind(Tick::declare()?)?),
    );
    let mut executor = UnitTestExecutor::new_with_graph(
        graph.build().unwrap(),
        UnitTestExecutorConfig {
            start_time: at(100),
            virtual_pool_threads: ThreadPoolConfigList::try_from([1, 1]).unwrap(),
            node_executor_thread_count: std::num::NonZeroUsize::new(2).unwrap(),
            ..Default::default()
        },
    );
    let mut sender = executor.add_test_publisher(bindings.take_publisher(&publisher).unwrap());
    let capture = executor.add_test_subscriber(bindings.take_subscriber(&subscriber).unwrap());
    sender.send(1);
    let step = executor.step();
    assert_eq!((step.before, step.after), (at(100), at(110)));
    assert_eq!(executor.current_time(), at(110));
    sender.send(2);
    let mut stamps = Vec::new();
    assert_eq!(
        capture.consume(|_, message| stamps.push(message.header.published_at)),
        2
    );
    assert_eq!(stamps, [at(100), at(110)]);
}

#[test]
fn capture_reports_overflow_and_indexes_each_batch_from_zero() {
    let mut plan = ChannelPlan::<u64>::new("fixture_only");
    let publisher = plan.publisher(1);
    let subscriber = plan.subscriber(1);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind();
    let mut executor = UnitTestExecutor::new_with_graph(
        GraphBuilder::with_channel_names([storage.name()])
            .build()
            .unwrap(),
        Default::default(),
    );
    let mut sender = executor.add_test_publisher(bindings.take_publisher(&publisher).unwrap());
    let capture = executor.add_test_subscriber(bindings.take_subscriber(&subscriber).unwrap());
    sender.send(1);
    sender.send(2);
    let (count, dropped) = capture.try_consume(|index, message| {
        assert_eq!((index, message.message), (0, 2));
    });
    assert_eq!((count, dropped.writer, dropped.reader), (1, 1, 0));
    drop(executor);
    sender.send(3);
    assert_eq!(
        capture
            .try_consume(|index, m| assert_eq!((index, m.message), (0, 3)))
            .0,
        1
    );
    assert_eq!(capture.try_consume(|_, _| panic!("empty batch")).0, 0);
}

struct Fails;
#[task_callback]
impl Fails {
    fn run(&self, output: Output<u64>) -> Result<(), LoanError> {
        output.write(7).send();
        Err(LoanError::PendingBatchNotEmpty)
    }
}
#[test]
fn failed_step_cancels_capture_and_poisons_executor() {
    let mut plan = ChannelPlan::new("failure");
    let declaration = Fails::declare(&mut plan).unwrap();
    let capture = plan.subscriber(1);
    let sender = plan.publisher(1);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind();
    let mut graph = GraphBuilder::with_channel_names([storage.name()]);
    let mut schedule = CallbackSchedule::periodic(Duration::from_nanos(10))
        .with_execution_duration(Duration::ZERO);
    schedule.run_on_start = false;
    graph.add_scheduled_callback(
        "fails",
        schedule,
        || Ok(Fails.bind(declaration, &bindings)?),
    );
    let mut executor = UnitTestExecutor::new_with_graph(graph.build().unwrap(), Default::default());
    let capture = executor.add_test_subscriber(bindings.take_subscriber(&capture).unwrap());
    let mut sender = executor.add_test_publisher(bindings.take_publisher(&sender).unwrap());
    assert_eq!(executor.step().after, at(10));
    assert!(matches!(
        executor.try_step(),
        Err(StepError::Callback { .. })
    ));
    assert_eq!(capture.message_count(), 0);
    sender.send(9);
    let mut observed = None;
    assert_eq!(
        capture.consume(|_, message| {
            observed = Some((message.message, message.header.published_at))
        }),
        1
    );
    assert_eq!(observed, Some((9, at(10))));
    assert!(matches!(executor.try_step(), Err(StepError::Poisoned)));
    assert!(sender.try_send_at(at(20), 10).is_err());
}

#[test]
fn invalid_configuration_is_reported_by_graph_constructor() {
    let graph = GraphBuilder::new().build().unwrap();
    let result = UnitTestExecutor::try_new_with_graph(
        graph,
        UnitTestExecutorConfig {
            virtual_pool_threads: ThreadPoolConfigList::new(),
            ..Default::default()
        },
    );
    assert!(matches!(result, Err(StepError::InvalidConfig(_))));
    let mut graph = GraphBuilder::new();
    graph.add_callback("tick", || Ok(|_: FrameworkTime| Ok(())));
    let mut graph = graph.build().unwrap();
    graph.configure_schedules(|_, schedule| {
        *schedule =
            CallbackSchedule::periodic(Duration::ZERO).with_execution_duration(Duration::ZERO);
    });
    assert!(matches!(
        UnitTestExecutor::try_new_with_graph(graph, Default::default()),
        Err(StepError::InvalidConfig(_))
    ));
}
