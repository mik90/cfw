use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use task::{
    CallbackSchedule, Context, InputSpan, LoanError, Publisher, RequiredInput, time::FrameworkTime,
};
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
struct BadBinding {
    drops: Arc<AtomicUsize>,
    key: task::PublisherKey<Counted>,
}
impl task::automatic::Task for BadFactory {
    fn register(
        self: Box<Self>,
        plan: &mut task::automatic::NamedPlan,
    ) -> Result<Box<dyn task::automatic::TaskFactory>, testing::TestBuildError> {
        Ok(Box::new(BadBinding {
            drops: self.0,
            key: plan.publisher("failed_output", 1)?,
        }))
    }
}
impl task::automatic::TaskFactory for BadBinding {
    fn build<'a>(
        self: Box<Self>,
        bindings: &task::automatic::NamedBindings<'a>,
    ) -> Result<Box<dyn task::Callback + 'a>, testing::TestBuildError> {
        let mut publisher = bindings
            .native::<Counted>("failed_output")?
            .take_publisher(&self.key)?;
        publisher
            .publish(Counted {
                value: 1,
                drops: self.drops,
            })
            .unwrap();
        Err(task::automatic::BuildError(
            "deliberate binding failure".into(),
        ))
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
