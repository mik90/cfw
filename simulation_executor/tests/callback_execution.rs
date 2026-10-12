use task::time::FrameworkTime;
use task::{
    BatchExecutionError, BatchFailure, Callback, CallbackNode, ChannelPlan, Context, GraphPlan,
    LoanError, Publisher, execute_callback, execute_callback_batch,
};

#[test]
fn rejected_batch_commit_cancels_outputs_before_single_callback_reuse() {
    let mut plan = ChannelPlan::<u64>::new("output");
    let source = plan.publisher(1);
    let capture = plan.subscriber(1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let capture = bindings.take_subscriber(&capture).unwrap();
    let mut callback = CallbackNode::new(
        (),
        bindings.take_publisher(&source).unwrap(),
        |_: &(), output: &mut Publisher<'_, u64>, _: &Context| output.publish(42),
    );
    let channels = Default::default();
    let callbacks = Default::default();
    let context = Context::new(FrameworkTime::from_nanoseconds(7), &channels, &callbacks);
    let result = execute_callback_batch(
        [(17, &mut callback as &mut dyn Callback)],
        &context,
        1,
        |executed| {
            assert_eq!(executed, [17]);
            capture.update();
            assert!(capture.input().pop().is_none());
            Err::<(), _>("timing rejected")
        },
    );
    assert!(matches!(
        result,
        Err(BatchExecutionError::BeforeCommit("timing rejected"))
    ));
    capture.update();
    assert!(capture.input().pop().is_none());
    assert!(execute_callback(&mut callback, &context).unwrap());
    capture.update();
    let retained = capture.input().pop().unwrap();
    drop(callback);
    assert_eq!(retained.message, 42);
    assert_eq!(retained.header.published_at, context.now());
}

#[test]
fn batch_failure_reports_the_supplied_id_after_skipping_unready_callbacks() {
    struct Gated;
    impl Callback for Gated {
        fn required_inputs_ready(&self) -> bool {
            false
        }
        fn run(&mut self, _: &Context) -> Result<(), LoanError> {
            panic!("gated body must not run")
        }
    }
    let mut gated = Gated;
    let mut failing = |_| Err(LoanError::ArenaExhausted);
    let channels = Default::default();
    let callbacks = Default::default();
    let context = Context::new(FrameworkTime::from_nanoseconds(0), &channels, &callbacks);
    let result = execute_callback_batch(
        [
            (10, &mut gated as &mut dyn Callback),
            (99, &mut failing as &mut dyn Callback),
        ],
        &context,
        2,
        |_| -> Result<(), ()> { panic!("failed bodies must not reach commit validation") },
    );
    assert!(matches!(
        result,
        Err(BatchExecutionError::Callback {
            index: 99,
            failure: BatchFailure::Callback(LoanError::ArenaExhausted)
        })
    ));
}
