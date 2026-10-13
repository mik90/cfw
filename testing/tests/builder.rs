use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use task::{Context, InputSpan, LoanError, Publisher, RequiredInput, time::FrameworkTime};
use task_graph::CallbackSchedule;
use task_macros::task_callback;
use testing::{ExecutionDuration, UnitTestExecutorBuilder, UnitTestExecutorConfig};

fn ns(n: u64) -> Duration {
    Duration::from_nanos(n)
}
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
fn feedback_captures_exclude_fixture_publications_without_breaking_feedback() {
    let mut builder = UnitTestExecutorBuilder::new();
    builder
        .add_task("double", Double, ns(1))
        .input_channel("input", "loop")
        .output_channel("output", "loop");
    let mut input = builder.add_test_publisher::<u64>("loop");
    let output = builder.add_test_subscriber::<u64>("loop");
    let all = builder.add_test_subscriber_with_sources::<u64>(
        "loop",
        8,
        testing::CaptureSources::AllPublishers,
    );
    builder.run(|mut executor| {
        input.send(3);
        executor.step();
        let mut outputs = Vec::new();
        output.messages(&mut executor, |_, message| outputs.push(message.message));
        assert_eq!(outputs, [6]);
        let mut publications = Vec::new();
        all.messages(&mut executor, |_, message| {
            publications.push(message.message)
        });
        publications.sort();
        assert_eq!(publications, [3, 6]);
        executor.step();
        assert_eq!(
            output.messages(&mut executor, |_, message| assert_eq!(message.message, 12)),
            1
        );
    });
}

struct Narrow;

struct ScheduledBatch;
#[task_callback]
impl ScheduledBatch {
    fn run(
        &self,
        #[capacity(4)] mut input: InputSpan<u64>,
        output: &mut Publisher<Vec<u64>>,
    ) -> Result<(), LoanError> {
        output.publish(input.drain().map(|message| message.message).collect())
    }
}

#[test]
fn scheduled_inputs_wait_for_their_deadline_and_respect_virtual_pool_capacity() {
    for threads in [1, 2] {
        let mut builder = UnitTestExecutorBuilder::with_config(UnitTestExecutorConfig {
            virtual_pool_threads: vec![threads],
            ..Default::default()
        });
        for name in ["left", "right"] {
            builder
                .add_task(name, ScheduledBatch, ns(5))
                .output_channel("output", name);
        }
        let mut input = builder.add_test_publisher::<u64>("input");
        let left = builder.add_test_subscriber::<Vec<u64>>("left");
        let right = builder.add_test_subscriber::<Vec<u64>>("right");
        builder.run(|mut executor| {
            assert!(input.try_send_at(FrameworkTime::INVALID, 0).is_err());
            input.send_at(at(30), 4);
            input.send_at(at(10), 1);
            input.send_at(at(10), 2);
            executor.run_until(at(10), 8).unwrap();
            assert_eq!(
                left.messages(&mut executor, |_, _| panic!("published early")),
                0
            );
            assert_eq!(
                right.messages(&mut executor, |_, _| panic!("published early")),
                0
            );
            // Insert a new deadline ahead of an already scheduled publication.
            input.send_at(at(20), 3);
            let steps = executor.run_until_idle(16).unwrap();
            assert_eq!(steps[0].before, at(10));
            assert_eq!(steps[0].executed.len(), threads);
            for (capture, delay) in [(&left, 0), (&right, if threads == 1 { 5 } else { 0 })] {
                let mut actual = Vec::new();
                capture.messages(&mut executor, |_, message| {
                    actual.push((message.header.published_at, message.message.clone()));
                });
                assert_eq!(
                    actual,
                    [
                        (at(10 + delay), vec![1, 2]),
                        (at(20 + delay), vec![3]),
                        (at(30 + delay), vec![4])
                    ]
                );
            }
            assert!(input.try_send_at(at(10), 9).is_err());
            // Input can restart an idle executor.
            input.send_at(at(50), 5);
            executor.run_until_idle(8).unwrap();
            assert_eq!(
                left.messages(&mut executor, |_, message| assert_eq!(
                    message.header.published_at,
                    at(50)
                )),
                1
            );
        });
        assert!(input.try_send_at(at(100), 9).is_err());
    }
}

#[task_callback]
impl Narrow {
    fn run(&self, input: RequiredInput<u32>) {
        let _ = input;
    }
}
#[test]
fn topology_exposes_ports_capacities_and_conflicting_declarations() {
    let mut builder = UnitTestExecutorBuilder::new();
    builder
        .add_task("double\"quoted", Double, ns(1))
        .input_channel("input", "loop")
        .output_channel("output", "loop");
    builder.add_test_publisher::<u64>("loop");
    builder.add_test_subscriber::<u64>("loop");
    let setup = builder.allocate();
    let topology = setup.topology();
    assert_eq!(topology.ports.len(), 2);
    assert_eq!(topology.ports[0].port, "input");
    assert_eq!(topology.ports[1].port, "output");
    assert_eq!(topology.channels[0].sources[1], Some(vec![0]));
    assert!(topology.to_string().contains("capacity="));
    assert!(topology.dot().contains("double\\\"quoted"));
    let mut builder = UnitTestExecutorBuilder::new();
    builder.add_task("wide", Double, ns(1));
    builder.add_task("narrow", Narrow, ns(1));
    let error = builder.try_allocate().err().unwrap().to_string();
    for word in ["wide", "narrow", "input", "u64", "u32"] {
        assert!(error.contains(word), "{error}");
    }
}

#[test]
fn scoped_and_explicit_forms_share_named_binding_and_send_time() {
    for scoped in [false, true] {
        let mut builder = UnitTestExecutorBuilder::with_config(UnitTestExecutorConfig {
            start_time: at(100),
            ..Default::default()
        });
        let mut input = builder.add_test_publisher::<u64>("input");
        let output = builder.add_test_subscriber::<u64>("output");
        builder.add_task("double", Double, ns(10));
        assert!(
            input
                .try_send(21)
                .unwrap_err()
                .to_string()
                .contains("not active")
        );
        let mut check = |mut executor: testing::UnitTestExecutor<'_>| {
            input.send(21);
            let step = executor.step();
            assert_eq!(step.executed, [0]);
            assert_eq!((step.before, step.after), (at(100), at(110)));
            assert_eq!(
                output.messages(&mut executor, |index, m| {
                    assert_eq!((index, m.message, m.header.published_at), (0, 42, at(100)));
                }),
                1
            );
            input.send(22);
            executor.step();
            output.messages(&mut executor, |index, m| {
                assert_eq!((index, m.message, m.header.published_at), (0, 44, at(110)))
            });
            assert_eq!(
                output.messages(&mut executor, |_, _| panic!("empty batch")),
                0
            );
            executor.step_count().0
        };
        if scoped {
            assert_eq!(builder.run(check), 2);
        } else {
            let setup = builder.allocate();
            assert_eq!(check(setup.build()), 2);
            assert!(
                setup
                    .try_build()
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("already been built")
            );
        }
        assert!(
            input
                .try_send(0)
                .unwrap_err()
                .to_string()
                .contains("closed")
        );
    }
}

struct Batch;
struct BorrowedSource {
    value: usize,
    drops: Arc<AtomicUsize>,
}
impl Drop for BorrowedSource {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct BorrowingForward;
type BorrowedSourcePtr<'a> = task::ArenaReaderPtr<'a, task::message::Message<BorrowedSource>>;
#[task_callback]
impl BorrowingForward {
    fn run<'storage>(
        &self,
        mut input: task::Input<'_, 'storage, BorrowedSource, task::Forwardable>,
        #[forwards(source = input)] output: &mut Publisher<
            'storage,
            task::Forwarded<bool, BorrowedSourcePtr<'storage>>,
        >,
    ) -> Result<(), LoanError> {
        for source in input.drain() {
            output.publish(task::Forwarded::new(true, source))?;
        }
        Ok(())
    }
}
struct RetainForwarded(Arc<AtomicUsize>);
#[task_callback]
impl RetainForwarded {
    fn run<'storage>(
        &self,
        #[capacity(32)]
        #[keep_across_runs(true)]
        input: InputSpan<'_, 'storage, task::Forwarded<bool, BorrowedSourcePtr<'storage>>>,
    ) {
        let sum = input
            .inputs()
            .map(|message| message.message.forwarded_messages.message.value)
            .sum();
        self.0.store(sum, Ordering::SeqCst);
    }
}
#[test]
fn named_borrowed_tasks_budget_nonclone_sources_and_destroy_retained_values() {
    let drops = Arc::new(AtomicUsize::new(0));
    let sum = Arc::new(AtomicUsize::new(0));
    let mut builder = UnitTestExecutorBuilder::new();
    builder
        .add_task("forward", BorrowingForward, ns(1))
        .input_channel("input", "source")
        .output_channel("output", "borrowed");
    builder
        .add_task("retain", RetainForwarded(sum.clone()), ns(1))
        .input_channel("input", "borrowed");
    let mut input = builder.add_test_publisher::<BorrowedSource>("source");
    builder.run(|mut executor| {
        for value in 0..16 {
            input.send(BorrowedSource {
                value,
                drops: drops.clone(),
            });
            for _ in 0..3 {
                executor.step();
            }
        }
        assert_eq!(sum.load(Ordering::SeqCst), (0..16).sum::<usize>());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    });
    assert_eq!(drops.load(Ordering::SeqCst), 16);
}
#[task_callback]
impl Batch {
    fn run(
        &self,
        #[capacity(32)] mut input: InputSpan<u64>,
        #[capacity(32)] output: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        for m in input.drain() {
            output.publish(m.message)?;
        }
        Ok(())
    }
}
#[test]
fn arbitrary_input_handle_count_and_fanout_fanin() {
    let mut builder = UnitTestExecutorBuilder::with_config(UnitTestExecutorConfig {
        virtual_pool_threads: vec![2],
        node_executor_thread_count: 2,
        ..Default::default()
    });
    builder.add_task("first", Batch, ns(3));
    builder.add_task("second", Batch, ns(5));
    let mut inputs: Vec<_> = (0..20)
        .map(|_| builder.add_test_publisher::<u64>("input"))
        .collect();
    let output = builder.add_test_subscriber_with_capacity::<u64>("output", 40);
    builder.run(move |mut executor| {
        for (index, input) in inputs.iter_mut().enumerate() {
            input.send(index as u64);
        }
        assert_eq!(executor.step().executed, [0, 1]);
        assert_eq!(
            output.messages(&mut executor, |index, m| assert_eq!(
                m.message,
                (index % 20) as u64
            )),
            40
        );
    });
}

struct Bridge;
#[task_callback]
impl Bridge {
    fn run(
        &self,
        #[channel("output")] input: RequiredInput<u64>,
        result: &mut Publisher<u64>,
    ) -> Result<(), LoanError> {
        result.publish(*input + 1)
    }
}
#[test]
fn same_type_channels_are_independent_and_tasks_wire_automatically() {
    let mut builder = UnitTestExecutorBuilder::new();
    builder.add_task("double", Double, ns(10));
    builder.add_task("bridge", Bridge, ns(20));
    let mut input = builder.add_test_publisher::<u64>("input");
    let middle = builder.add_test_subscriber::<u64>("output");
    let result = builder.add_test_subscriber::<u64>("result");
    builder.run(|mut executor| {
        input.send(21);
        assert_eq!(executor.step().executed, [0]);
        assert_eq!(
            result.messages(&mut executor, |_, _| panic!("premature downstream result")),
            0
        );
        middle.messages(&mut executor, |_, m| assert_eq!(m.message, 42));
        assert_eq!(executor.step().executed, [1]);
        result.messages(&mut executor, |_, m| {
            assert_eq!((m.message, m.header.published_at), (43, at(10)))
        });
        assert_eq!(executor.current_time(), at(30));
    });
}

struct Counted {
    value: u64,
    drops: Arc<AtomicUsize>,
}
impl Drop for Counted {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct NonClone;
#[task_callback]
impl NonClone {
    fn run(
        &self,
        #[keep_across_runs(false)] input: RequiredInput<Counted>,
        output: &mut Publisher<Counted>,
    ) -> Result<(), LoanError> {
        output.publish(Counted {
            value: input.value * 2,
            drops: input.drops.clone(),
        })
    }
}
#[test]
fn nonclone_inspection_and_assertion_panics_release_queued_and_arena_values() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut builder = UnitTestExecutorBuilder::new();
    builder.add_task("nonclone", NonClone, ns(1));
    let mut input = builder.add_test_publisher::<Counted>("input");
    let output = builder.add_test_subscriber::<Counted>("output");
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        builder.run(|mut executor| {
            input.send(Counted {
                value: 21,
                drops: drops.clone(),
            });
            executor.step();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(
                catch_unwind(AssertUnwindSafe(|| output.messages(
                    &mut executor,
                    |index, m| {
                        assert_eq!((index, m.message.value), (0, 42));
                        panic!("inspection assertion");
                    }
                )))
                .is_err()
            );
            assert_eq!(drops.load(Ordering::SeqCst), 2);
            assert_eq!(
                output.messages(&mut executor, |_, _| panic!("already consumed")),
                0
            );
            input.send(Counted {
                value: 3,
                drops: drops.clone(),
            });
            panic!("test assertion with queued input");
        })
    }));
    assert!(outcome.is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    assert!(
        input
            .try_send(Counted {
                value: 0,
                drops: drops.clone()
            })
            .is_err()
    );
    assert_eq!(drops.load(Ordering::SeqCst), 4);
}

#[test]
fn capture_capacity_and_cumulative_overflow_are_preserved() {
    let mut builder = UnitTestExecutorBuilder::new();
    builder.add_task("double", Double, ns(1));
    let mut input = builder.add_test_publisher::<u64>("input");
    let default_capture = builder.add_test_subscriber::<u64>("output");
    let small = builder.add_test_subscriber_with_capacity::<u64>("output", 1);
    builder.run(|mut executor| {
        for value in 0..10 {
            input.send(value);
            executor.step();
        }
        assert_eq!(
            default_capture.messages(&mut executor, |i, m| assert_eq!(m.message, i as u64 * 2)),
            10
        );
        let (count, drops) =
            small.try_messages(&mut executor, |i, m| assert_eq!((i, m.message), (0, 18)));
        assert_eq!((count, drops.writer, drops.reader), (1, 9, 0));
        input.send(10);
        executor.step();
        assert_eq!(
            small
                .try_messages(&mut executor, |i, m| assert_eq!((i, m.message), (0, 20)))
                .1
                .writer,
            9
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| small.messages(&mut executor, |_, _| {}))).is_err()
        );
    });
}

#[test]
fn setup_errors_close_handles_and_foreign_outputs_are_rejected() {
    let mut missing = UnitTestExecutorBuilder::new();
    let mut handle = missing.add_test_publisher::<u64>("missing");
    missing.add_test_subscriber::<u64>("missing");
    assert!(
        missing
            .try_allocate()
            .err()
            .unwrap()
            .to_string()
            .contains("no task subscriber")
    );
    assert!(
        handle
            .try_send(1)
            .unwrap_err()
            .to_string()
            .contains("closed")
    );
    let mut wrong = UnitTestExecutorBuilder::new();
    wrong.add_task("double", Double, ns(1));
    wrong.add_test_publisher::<String>("input");
    assert!(
        wrong
            .try_run(|_| ())
            .unwrap_err()
            .to_string()
            .contains("incompatible payload")
    );
    let mut duplicate = UnitTestExecutorBuilder::new();
    duplicate.add_task("same", Double, ns(1));
    duplicate.add_task("same", Double, ns(1));
    assert!(
        duplicate
            .try_allocate()
            .err()
            .unwrap()
            .to_string()
            .contains("duplicate task")
    );
    let mut invalid = UnitTestExecutorBuilder::with_config(UnitTestExecutorConfig {
        node_executor_thread_count: 0,
        ..Default::default()
    });
    invalid.add_task("double", Double, ns(1));
    let mut handle = invalid.add_test_publisher::<u64>("input");
    let setup = invalid.allocate();
    assert!(setup.try_build().is_err());
    assert!(
        handle
            .try_send(1)
            .unwrap_err()
            .to_string()
            .contains("closed")
    );
    let mut foreign = UnitTestExecutorBuilder::new();
    let output = foreign.add_test_subscriber::<u64>("output");
    UnitTestExecutorBuilder::new().run(|mut executor| {
        assert!(
            catch_unwind(AssertUnwindSafe(
                || output.messages(&mut executor, |_, _| {})
            ))
            .is_err()
        );
    });
    assert!(catch_unwind(AssertUnwindSafe(|| foreign.allocate())).is_err());
}

struct Tick {
    calls: Arc<AtomicUsize>,
}
#[task_callback]
impl Tick {
    fn run(&self, output: &mut Publisher<u64>, context: &Context) -> Result<(), LoanError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        output.publish(context.now().to_nanoseconds() as u64)
    }
}
#[test]
fn explicit_fixed_dynamic_durations_pools_and_completion_deadlines() {
    let calls = Arc::new(AtomicUsize::new(0));
    let durations = Arc::new(AtomicUsize::new(0));
    let mut builder = UnitTestExecutorBuilder::with_config(UnitTestExecutorConfig {
        virtual_pool_threads: vec![1, 1],
        node_executor_thread_count: 2,
        ..Default::default()
    });
    let finished = calls.clone();
    let durations_used = durations.clone();
    builder.add_scheduled_task(
        "dynamic",
        Tick {
            calls: calls.clone(),
        },
        ExecutionDuration::dynamic(move || {
            durations_used.fetch_add(1, Ordering::SeqCst);
            ns(2)
        }),
        CallbackSchedule::on_start()
            .in_pool(1)
            .with_next_execution_time_callback(move |now| {
                (finished.load(Ordering::SeqCst) < 2).then(|| now + ns(5))
            }),
    );
    builder.add_scheduled_task(
        "fixed",
        Tick {
            calls: Arc::new(AtomicUsize::new(0)),
        },
        ns(10),
        CallbackSchedule::on_start(),
    );
    builder.add_scheduled_task(
        "contender",
        Tick {
            calls: Arc::new(AtomicUsize::new(0)),
        },
        ns(3),
        CallbackSchedule::on_start(),
    );
    let output = builder.add_test_subscriber::<u64>("output");
    let setup = builder.allocate();
    let mut executor = setup.build();
    assert_eq!(
        durations.load(Ordering::SeqCst),
        0,
        "construction must not execute duration callbacks"
    );
    let mut invocations = Vec::new();
    for _ in 0..10 {
        let step = executor.step();
        output.messages(&mut executor, |_, m| invocations.push(m.message));
        if step.idle {
            break;
        }
    }
    assert_eq!(invocations, [0, 0, 7, 10]);
    assert_eq!(durations.load(Ordering::SeqCst), 2);
    assert_eq!(executor.current_time(), at(13));
}

struct Fails;
#[task_callback]
impl Fails {
    fn run(&self, input: RequiredInput<u64>, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(*input)?;
        Err(LoanError::LoanCapacityReached)
    }
}
#[test]
fn failed_step_cancels_outputs_and_closes_injection_handles() {
    let mut builder = UnitTestExecutorBuilder::new();
    builder.add_task("fails", Fails, ns(1));
    let mut input = builder.add_test_publisher::<u64>("input");
    let output = builder.add_test_subscriber::<u64>("output");
    builder.run(|mut executor| {
        input.send(1);
        assert!(executor.try_step().is_err());
        assert!(input.try_send(2).is_err());
        assert_eq!(
            output.messages(&mut executor, |_, _| panic!("uncommitted")),
            0
        );
        assert!(matches!(
            executor.try_step(),
            Err(simulation_executor::StepError::Poisoned)
        ));
    });
}

struct BadFactory(Arc<AtomicUsize>);
impl task_graph::Task for BadFactory {
    fn register(
        self: Box<Self>,
        plan: &mut task_graph::GraphPlan,
        overrides: &task_graph::ChannelOverrides,
    ) -> Result<task_graph::TaskBinding, testing::TestBuildError> {
        overrides.validate(&[])?;
        let key = plan.publisher("failed_output", 1)?;
        Ok(Box::new(move |bindings| {
            let mut publisher = bindings
                .native::<Counted>("failed_output")?
                .take_publisher(&key)?;
            publisher
                .publish(Counted {
                    value: 1,
                    drops: self.0,
                })
                .unwrap();
            Err(task_graph::BuildError("deliberate binding failure".into()))
        }))
    }
}
#[test]
fn partial_construction_failure_drops_pending_loans_and_closes_handles() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut builder = UnitTestExecutorBuilder::new();
    builder.add_task("double", Double, ns(1));
    builder.add_task("broken", BadFactory(drops.clone()), ns(1));
    let mut input = builder.add_test_publisher::<u64>("input");
    let setup = builder.allocate();
    let error = setup.try_build().err().unwrap().to_string();
    assert!(
        error.contains("broken") && error.contains("deliberate binding failure"),
        "{error}"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(input.try_send(1).is_err());
}
