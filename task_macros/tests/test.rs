use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use task::time::FrameworkTime;
use task::{
    ChannelPlan, Context, GraphBuilder, GraphPlan, Input, InputSpan, LoanError, Output, OutputSpan,
    OutputUninit, Publisher, RequiredInput,
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

#[test]
fn arrival_after_input_snapshot_does_not_make_an_empty_required_view_ready() {
    struct LateArrival<C, F> {
        inner: C,
        publish: F,
    }
    impl<C: task::Callback, F: FnMut() + Send> task::Callback for LateArrival<C, F> {
        fn update_inputs(&mut self) {
            self.inner.update_inputs();
            (self.publish)();
        }
        fn required_inputs_available(&self) -> bool {
            self.inner.required_inputs_available()
        }
        fn required_inputs_ready(&self) -> bool {
            self.inner.required_inputs_ready()
        }
        fn run(&mut self, ctx: &Context) -> Result<(), LoanError> {
            self.inner.run(ctx)
        }
        fn flush_outputs(&mut self, time: FrameworkTime) {
            self.inner.flush_outputs(time);
        }
        fn discard_outputs(&mut self) {
            self.inner.discard_outputs();
        }
        fn finish_inputs(&mut self) {
            self.inner.finish_inputs();
        }
    }
    let mut input = ChannelPlan::new("in");
    let mut output = ChannelPlan::new("out");
    let declaration = Increment::declare(&mut input, &mut output).unwrap();
    let source_key = input.publisher(1);
    let capture_key = output.subscriber(1);
    let storage = GraphPlan::new((input, output)).allocate().unwrap();
    let input = storage.channels().0.build();
    let output = storage.channels().1.build();
    let mut source = input.take_publisher(&source_key).unwrap();
    let capture = output.take_subscriber(&capture_key).unwrap();
    let mut callback = LateArrival {
        inner: Increment.bind(declaration, &input, &output).unwrap(),
        publish: move || {
            source.publish(32).unwrap();
            source.flush(FrameworkTime::from_nanoseconds(0));
        },
    };
    let channels = Default::default();
    let callbacks = Default::default();
    let context = Context::new(FrameworkTime::from_nanoseconds(10), &channels, &callbacks);
    assert!(!task::execute_callback(&mut callback, &context).unwrap());
    assert!(task::Callback::required_inputs_available(&callback));
    assert!(!task::Callback::required_inputs_ready(&callback));
    assert!(task::execute_callback(&mut callback, &context).unwrap());
    capture.update();
    assert_eq!(capture.input().pop().unwrap().message, 42);
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

// Intentionally neither Clone nor Default: spans inspect/initialize real payloads.
struct SpanValue(u64);
struct SpanTransform {
    runs: usize,
}
#[task_callback]
impl SpanTransform {
    fn run(
        &mut self,
        #[channel("span_in")]
        #[capacity(3)]
        mut input: InputSpan<SpanValue>,
        #[channel("span_out")]
        #[capacity(3)]
        output: OutputSpan<SpanValue>,
    ) -> Result<(), LoanError> {
        self.runs += 1;
        if self.runs == 3 {
            assert!(input.is_empty());
            return Ok(());
        }
        assert_eq!(input.len(), 3);
        for _ in 0..2 {
            assert_eq!(
                input
                    .inputs()
                    .map(|m| (m.message.0, m.header.published_at.to_nanoseconds()))
                    .collect::<Vec<_>>(),
                [(5, 100), (7, 101), (11, 102)]
            );
        }
        let mut loans = Vec::new();
        for message in input.inputs() {
            let loan = output.loan_uninit()?;
            loans.push(loan.write(SpanValue(message.message.0 * 2)));
        }
        assert!(matches!(
            output.loan_uninit(),
            Err(LoanError::LoanCapacityReached)
        ));
        for loan in loans.into_iter().rev() {
            loan.send();
        }
        if self.runs == 2 {
            assert_eq!(input.drain().count(), 3);
        }
        Ok(())
    }
}

#[test]
fn generated_input_and_output_spans_retain_inputs_and_commit_in_send_order() {
    let mut input = ChannelPlan::new("span_in");
    let mut output = ChannelPlan::new("span_out");
    let declaration = SpanTransform::declare(&mut input, &mut output).unwrap();
    let source_key = input.publisher(1);
    let capture_key = output.subscriber(3);
    let storage = GraphPlan::new((input, output)).allocate().unwrap();
    let input = storage.channels().0.build();
    let output = storage.channels().1.build();
    let mut source = input.take_publisher(&source_key).unwrap();
    let capture = output.take_subscriber(&capture_key).unwrap();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_callback("spans", || {
        Ok(SpanTransform { runs: 0 }.bind(declaration, &input, &output)?)
    });
    let mut graph = graph.build().unwrap();
    for (index, value) in [5, 7, 11].into_iter().enumerate() {
        source.publish(SpanValue(value)).unwrap();
        source.flush(FrameworkTime::from_nanoseconds(100 + index as i64));
    }
    assert!(capture.input().is_empty());
    for time in [200, 201] {
        graph.step(FrameworkTime::from_nanoseconds(time)).unwrap();
        capture.update();
        let values: Vec<_> = capture
            .input()
            .drain()
            .map(|m| (m.message.0, m.header.published_at.to_nanoseconds()))
            .collect();
        assert_eq!(values, [(22, time), (14, time), (10, time)]);
    }
    graph.step(FrameworkTime::from_nanoseconds(202)).unwrap();
    capture.update();
    assert!(capture.input().is_empty());
}

struct SpanFailure {
    drops: Arc<AtomicUsize>,
    panic: bool,
}
#[task_callback]
impl SpanFailure {
    fn run(&self, #[capacity(3)] output: OutputSpan<Counted>) -> Result<(), LoanError> {
        let first = output.loan_uninit()?;
        let _unsent = output.loan(Counted(Some(self.drops.clone())))?;
        first.write(Counted(Some(self.drops.clone()))).send();
        let _uninitialized = output.loan_uninit()?;
        assert!(!self.panic, "span callback failed");
        Err(LoanError::LoanCapacityReached)
    }
}

#[test]
fn generated_span_outputs_cancel_sent_and_unsent_loans_on_failure() {
    for panic in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut plan = ChannelPlan::new("span");
        let declaration = SpanFailure::declare(&mut plan).unwrap();
        let capture_key = plan.subscriber(3);
        let storage = GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build();
        let capture = bindings.take_subscriber(&capture_key).unwrap();
        let mut callback = SpanFailure {
            drops: drops.clone(),
            panic,
        }
        .bind(declaration, &bindings)
        .unwrap();
        for invocation in 1..=2 {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                task::execute_callback(
                    &mut callback,
                    &Context::new(
                        FrameworkTime::from_nanoseconds(invocation),
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
            assert_eq!(drops.load(Ordering::Relaxed), invocation as usize * 2);
            capture.update();
            assert!(capture.input().is_empty());
        }
    }
}

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

#[test]
fn named_forwarding_retention_fanout_and_rebinding_preserve_source_lifetimes() {
    use task::automatic::{NamedPlan, TaskRegistration};
    let mut plan = NamedPlan::default();
    let mut registration =
        TaskRegistration::new("forward", Forward, task::CallbackSchedule::default());
    registration
        .input_channel("input", "source")
        .output_channel("output", "forwarded");
    let forward = registration.register(&mut plan).unwrap();
    let injection = plan.native::<u64>("source").unwrap().publisher(1);
    let capture = plan
        .forwarded::<bool, u64>("forwarded")
        .unwrap()
        .subscriber(32);
    let fanout = plan
        .forwarded::<bool, u64>("forwarded")
        .unwrap()
        .subscriber(32);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    assert!(
        bindings
            .native::<task::ForwardedMessage<'static, bool, u64>>("forwarded")
            .is_err()
    );
    let mut source = bindings
        .native::<u64>("source")
        .unwrap()
        .take_publisher(&injection)
        .unwrap();
    let capture = bindings
        .forwarded::<bool, u64>("forwarded")
        .unwrap()
        .take_subscriber(&task::automatic::forwarding::subscriber_key(capture))
        .unwrap();
    let fanout = bindings
        .forwarded::<bool, u64>("forwarded")
        .unwrap()
        .take_subscriber(&task::automatic::forwarding::subscriber_key(fanout))
        .unwrap();
    let mut graph = storage.graph_builder();
    forward.add_to_graph(&mut graph, &bindings);
    let mut graph = graph.build().unwrap();
    for value in 0..32 {
        source.publish(value).unwrap();
        source.flush(FrameworkTime::from_nanoseconds(value as i64));
        graph
            .step(FrameworkTime::from_nanoseconds(value as i64))
            .unwrap();
        capture.update();
        fanout.update();
    }
    let retained: Vec<_> = capture.input().drain().collect();
    assert_eq!(retained.len(), 32);
    let shared: Vec<_> = fanout.input().drain().collect();
    drop((graph, source, capture, fanout, bindings));
    let second = storage.bind().unwrap();
    assert!(second.forwarded::<bool, u64>("forwarded").is_ok());
    drop(second);
    for (index, message) in retained.iter().enumerate() {
        assert_eq!(message.message.forwarded.message, index as u64);
        assert_eq!(shared[index].message.forwarded.message, index as u64);
    }
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
