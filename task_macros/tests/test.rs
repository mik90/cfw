use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use task::time::FrameworkTime;
use task::{
    ChannelPlan, Context, GraphBuilder, GraphPlan, Input, LoanError, Output, OutputUninit,
    Publisher, RequiredInput,
};
use task_macros::task_callback;

struct Increment;

#[task_callback]
impl Increment {
    fn run(
        &self,
        #[channel("in")] mut input: RequiredInput<i32>,
        #[channel("out")] mut output: Output<i32>,
        ctx: &Context,
    ) {
        assert_eq!(ctx.now(), FrameworkTime::from_nanoseconds(10));
        *output = *input + 10;
        input.clear();
        output.send();
    }
}

#[test]
fn generated_callback_declares_then_binds_and_runs_with_executor_owned_io() {
    let mut input = ChannelPlan::new("in");
    let mut output = ChannelPlan::new("out");
    let declaration = Increment::declare(&mut input, &mut output).unwrap();
    let source_key = input.publisher(1);
    let capture_key = output.subscriber(1);
    let storage = GraphPlan::new((input, output)).allocate().unwrap();
    let (mut graph, mut source, capture) = {
        let input = storage.channels().0.build();
        let output = storage.channels().1.build();
        let source = input.take_publisher(&source_key).unwrap();
        let capture = output.take_subscriber(&capture_key).unwrap();
        let mut builder = GraphBuilder::new();
        builder.add_callback("increment", || {
            Ok(Increment.bind(declaration, &input, &output)?)
        });
        (builder.build().unwrap(), source, capture)
    };
    let time = FrameworkTime::from_nanoseconds(10);
    graph.step(time).unwrap(); // Required input is empty; no output is loaned.
    capture.update();
    assert!(capture.input().pop().is_none());
    source.publish(32).unwrap();
    source.flush(time);
    std::thread::scope(|scope| {
        scope
            .spawn(move || graph.step(time).unwrap())
            .join()
            .unwrap();
    });
    capture.update();
    let retained = capture.input().pop().unwrap();
    drop((source, capture));
    assert_eq!(retained.message, 42);
    assert_eq!(retained.header.published_at, time);
}

#[test]
fn annotation_mismatch_is_reported_before_registering_any_ports() {
    let mut input = ChannelPlan::new("in");
    let mut output = ChannelPlan::new("wrong");
    let probe = input.publisher(1);
    let before = input.publisher_capacity(&probe).unwrap();
    let error = Increment::declare(&mut input, &mut output).err().unwrap();
    assert_eq!(error.field, "output");
    assert_eq!(error.expected, "out");
    assert_eq!(error.actual, "wrong");
    assert_eq!(input.publisher_capacity(&probe).unwrap(), before);
}

#[derive(Default)]
struct Counted(Option<Arc<AtomicUsize>>);
impl Drop for Counted {
    fn drop(&mut self) {
        if let Some(count) = &self.0 {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct Failure {
    drops: Arc<AtomicUsize>,
    panic: bool,
}
#[task_callback]
impl Failure {
    fn run(&mut self, mut output: Output<Counted>) -> Result<(), LoanError> {
        output.0 = Some(self.drops.clone());
        output.send();
        assert!(!self.panic, "callback failed");
        Err(LoanError::LoanCapacityReached)
    }
}

#[test]
fn generated_pending_outputs_are_discarded_on_error_and_panic() {
    for panic in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut output = ChannelPlan::new("output");
        let declaration = Failure::declare(&mut output).unwrap();
        let capture_key = output.subscriber(1);
        let storage = GraphPlan::new(output).allocate().unwrap();
        let bindings = storage.channels().build();
        let capture = bindings.take_subscriber(&capture_key).unwrap();
        let mut callback = Failure {
            drops: drops.clone(),
            panic,
        }
        .bind(declaration, &bindings)
        .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            task::execute_callback(
                &mut callback,
                &Context::new(
                    FrameworkTime::from_nanoseconds(0),
                    &Default::default(),
                    &Default::default(),
                ),
            )
        }));
        if panic {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), Err(LoanError::LoanCapacityReached));
        }
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        capture.update();
        assert!(capture.input().pop().is_none());
        // A second invocation also reaches user code; the pending loan was released.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            task::execute_callback(
                &mut callback,
                &Context::new(
                    FrameworkTime::from_nanoseconds(1),
                    &Default::default(),
                    &Default::default(),
                ),
            )
        }));
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }
}

struct InPlace;
#[task_callback]
impl InPlace {
    fn run(&mut self, mut output: OutputUninit<String>) {
        output.payload_uninit().write(String::from("initialized"));
        // SAFETY: The payload was initialized above; the loan initialized its header.
        unsafe { output.assume_init() }.send();
    }
}

#[test]
fn generated_uninitialized_output_publishes_on_success() {
    let mut plan = ChannelPlan::new("text");
    let declaration = InPlace::declare(&mut plan).unwrap();
    let capture_key = plan.subscriber(1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let capture = bindings.take_subscriber(&capture_key).unwrap();
    let mut callback = InPlace.bind(declaration, &bindings).unwrap();
    task::execute_callback(
        &mut callback,
        &Context::new(
            FrameworkTime::from_nanoseconds(3),
            &Default::default(),
            &Default::default(),
        ),
    )
    .unwrap();
    capture.update();
    assert_eq!(capture.input().pop().unwrap().message, "initialized");
}

struct Forward;
#[task_callback]
impl Forward {
    fn run<'storage>(
        &mut self,
        #[capacity(2)] mut input: Input<'_, 'storage, u64>,
        #[capacity(2)] output: &mut Publisher<
            'storage,
            task::ForwardedMessage<'storage, bool, u64>,
        >,
    ) -> Result<(), LoanError> {
        for ptr in input.drain() {
            output.publish(task::ForwardedMessage::new(true, ptr))?;
        }
        Ok(())
    }
}

#[test]
fn generated_forwarding_retains_storage_without_static_payload_bounds() {
    let mut source = ChannelPlan::new("source");
    let mut destination = ChannelPlan::new("destination");
    let declaration = Forward::declare(&mut source, &mut destination).unwrap();
    let input_key = source.publisher(2);
    let capture_key = destination.subscriber(2);
    source.reserve_retained(&input_key, 8).unwrap();
    let storage = GraphPlan::new((source, destination)).allocate().unwrap();
    let source = storage.channels().0.build();
    let destination = storage.channels().1.build();
    let mut fixture = source.take_publisher(&input_key).unwrap();
    let capture = destination.take_subscriber(&capture_key).unwrap();
    let mut callback = Forward.bind(declaration, &source, &destination).unwrap();
    fixture.publish(42).unwrap();
    fixture.flush(FrameworkTime::from_nanoseconds(0));
    task::execute_callback(
        &mut callback,
        &Context::new(
            FrameworkTime::from_nanoseconds(1),
            &Default::default(),
            &Default::default(),
        ),
    )
    .unwrap();
    capture.update();
    let retained = capture.input().pop().unwrap();
    drop((callback, fixture, capture, source, destination));
    assert_eq!(retained.message.forwarded.message, 42);
}

struct Feedback;
#[task_callback]
impl Feedback {
    fn run(&self, mut input: Input<u64>, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        if let Some(message) = input.pop() {
            output.publish(message.message + 1)?;
        }
        Ok(())
    }
}

#[test]
fn explicit_keys_allow_input_and_output_on_the_same_channel() {
    let mut plan = ChannelPlan::new("feedback");
    let declaration = FeedbackDeclaration::from_keys(plan.subscriber(1), plan.publisher(1));
    let fixture_key = plan.publisher(1);
    let capture_key = plan.subscriber(2);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut fixture = bindings.take_publisher(&fixture_key).unwrap();
    let capture = bindings.take_subscriber(&capture_key).unwrap();
    let mut callback = Feedback.bind(declaration, &bindings, &bindings).unwrap();
    fixture.publish(1).unwrap();
    fixture.flush(FrameworkTime::from_nanoseconds(0));
    task::execute_callback(
        &mut callback,
        &Context::new(
            FrameworkTime::from_nanoseconds(1),
            &Default::default(),
            &Default::default(),
        ),
    )
    .unwrap();
    capture.update();
    assert_eq!(
        capture
            .input()
            .drain()
            .map(|m| m.message)
            .collect::<Vec<_>>(),
        [1, 2]
    );
}
