use crossbeam::channel::{self, Receiver, Sender};
use live_executor::{LiveExecutor, LiveExecutorError, ThreadFailure};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use task::executor::TimeSource;
use task::time::FrameworkTime;
use task::{
    CallbackSchedule, ChannelPlan, Context, GraphBuilder, GraphPlan, LoanError, Output, Publisher,
    RequiredInput,
};
use task_macros::task_callback;

struct Clock;
impl TimeSource for Clock {
    fn now(&self) -> FrameworkTime {
        FrameworkTime::from_nanoseconds(1234)
    }
}
fn wait<T>(receiver: &Receiver<T>) -> T {
    receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("worker did not respond")
}

struct Increment {
    done: Sender<()>,
}
#[task_callback]
impl Increment {
    fn run(&mut self, mut input: RequiredInput<u64>, mut output: Output<u64>, ctx: &Context) {
        let channels = ctx.channel_names();
        for name in ["input", "output", "fixture-only"] {
            let id = channels.lookup_by_value(name).unwrap();
            assert_eq!(channels.lookup_by_id(id), name);
        }
        let id = ctx.callback_names().lookup_by_value("increment").unwrap();
        assert_eq!(ctx.callback_names().lookup_by_id(id), "increment");
        assert_eq!(ctx.now(), FrameworkTime::from_nanoseconds(1234));
        assert_eq!(std::thread::current().name(), Some("cfw_pool_1_t_0"));
        *output = *input + 1;
        input.clear();
        output.send();
        self.done.send(()).unwrap();
    }
}

#[test]
fn generated_callbacks_wake_in_assigned_pool_and_messages_outlive_execution() {
    let mut input = ChannelPlan::new("input");
    let mut output = ChannelPlan::new("output");
    let declaration = Increment::declare(&mut input, &mut output).unwrap();
    let source_key = input.publisher(1);
    let capture_key = output.subscriber(2);
    let storage = GraphPlan::new((input, (output, ChannelPlan::<u64>::new("fixture-only"))))
        .allocate()
        .unwrap();
    let input = storage.channels().0.build();
    let output = storage.channels().1.0.build();
    let mut source = input.take_publisher(&source_key).unwrap();
    let capture = output.take_subscriber(&capture_key).unwrap();
    let (done, completed) = channel::unbounded();
    let mut builder = GraphBuilder::with_storage(&storage);
    builder.add_scheduled_callback("increment", CallbackSchedule::default().in_pool(1), || {
        Ok(Increment { done }.bind(declaration, &input, &output)?)
    });
    let graph = builder.build().unwrap();
    assert!(Arc::ptr_eq(
        storage.channel_names(),
        &graph.metadata().channel_names
    ));
    drop((input, output));
    // A pre-start publication must not be lost when notifications are installed.
    source.publish(40).unwrap();
    source.flush(FrameworkTime::from_nanoseconds(0));
    let executor = LiveExecutor::new_multi_pool_with_time(vec![2, 1], graph, Clock).unwrap();
    let stop = executor.stop_signal();
    let main = std::thread::current().id();
    let result = executor
        .run_with(|_| {
            assert_eq!(std::thread::current().id(), main);
            wait(&completed);
            source.publish(41).unwrap();
            source.flush(FrameworkTime::from_nanoseconds(0));
            wait(&completed);
            17
        })
        .unwrap();
    assert_eq!(result, 17);
    assert!(stop.is_stopped());
    capture.update();
    let messages: Vec<_> = capture.input().drain().collect();
    drop((capture, source));
    assert_eq!(
        messages.iter().map(|m| m.message).collect::<Vec<_>>(),
        [41, 42]
    );
    assert!(
        messages
            .iter()
            .all(|m| m.header.published_at == FrameworkTime::from_nanoseconds(1234))
    );
    stop.request_stop(); // Safe after all borrowed execution objects are destroyed.
}

struct Blocking {
    entered: Sender<u64>,
    release: Receiver<()>,
    active: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}
#[task_callback]
impl Blocking {
    fn run(&mut self, mut input: RequiredInput<u64>) {
        assert_eq!(self.active.fetch_add(1, Ordering::SeqCst), 0);
        let value = *input;
        input.clear();
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.send(value).unwrap();
        if call == 0 {
            wait(&self.release);
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[test]
fn trigger_during_run_is_requeued_without_concurrent_invocations() {
    let mut plan = ChannelPlan::new("input");
    let declaration = Blocking::declare(&mut plan).unwrap();
    let source_key = plan.publisher(1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut source = bindings.take_publisher(&source_key).unwrap();
    let (entered, entry) = channel::unbounded();
    let (release, permission) = channel::bounded(1);
    let active = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut builder = GraphBuilder::new();
    builder.add_callback("blocking", || {
        Ok(Blocking {
            entered,
            release: permission,
            active: active.clone(),
            calls: calls.clone(),
        }
        .bind(declaration, &bindings)?)
    });
    let executor =
        LiveExecutor::new_multi_pool_with_time(vec![4], builder.build().unwrap(), Clock).unwrap();
    executor
        .run_with(|_| {
            source.publish(1).unwrap();
            source.flush(FrameworkTime::from_nanoseconds(0));
            assert_eq!(wait(&entry), 1);
            for value in 2..=4 {
                source.publish(value).unwrap();
                source.flush(FrameworkTime::from_nanoseconds(0));
            }
            assert_eq!(active.load(Ordering::SeqCst), 1);
            release.send(()).unwrap();
            assert_eq!(wait(&entry), 4);
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Source {
    drops: Arc<AtomicUsize>,
}
#[task_callback]
impl Source {
    fn run(&mut self, output: &mut Publisher<Counted>) -> Result<(), LoanError> {
        output.publish(Counted(self.drops.clone()))
    }
}
struct Failing {
    drops: Arc<AtomicUsize>,
    panic: bool,
}
#[task_callback]
impl Failing {
    fn run(
        &mut self,
        _input: RequiredInput<Counted>,
        output: &mut Publisher<Counted>,
    ) -> Result<(), LoanError> {
        output.publish(Counted(self.drops.clone()))?;
        assert!(!self.panic, "worker callback panic");
        Err(LoanError::LoanCapacityReached)
    }
}

#[test]
fn callback_errors_and_panics_wake_control_and_leave_captured_messages_valid() {
    for panic in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut source = ChannelPlan::new("source");
        let mut output = ChannelPlan::new("unpublished");
        let source_decl = Source::declare(&mut source).unwrap();
        let fail_decl = Failing::declare(&mut source, &mut output).unwrap();
        let capture_key = source.subscriber(1);
        let storage = GraphPlan::new((source, output)).allocate().unwrap();
        let source = storage.channels().0.build();
        let output = storage.channels().1.build();
        let capture = source.take_subscriber(&capture_key).unwrap();
        let mut builder = GraphBuilder::new();
        builder.add_scheduled_callback("source", CallbackSchedule::on_start(), || {
            Ok(Source {
                drops: drops.clone(),
            }
            .bind(source_decl, &source)?)
        });
        builder.add_callback("failure", || {
            Ok(Failing {
                drops: drops.clone(),
                panic,
            }
            .bind(fail_decl, &source, &output)?)
        });
        let executor =
            LiveExecutor::new_multi_pool_with_time(vec![3], builder.build().unwrap(), Clock)
                .unwrap();
        drop((source, output));
        match executor.run_with(|stop| stop.wait()) {
            Err(LiveExecutorError::Threads(failures)) => {
                assert_eq!(failures.len(), 1);
                if panic {
                    assert!(matches!(&failures[0], ThreadFailure::Panic { .. }));
                } else {
                    assert!(
                        matches!(&failures[0], ThreadFailure::Callback { callback, .. } if callback == "failure")
                    );
                }
            }
            _ => panic!("expected worker failure"),
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        capture.update();
        let retained = capture.input().pop().unwrap();
        drop(capture);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(retained);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn periodic_callbacks_run_until_controller_requests_stop() {
    struct AdvancingClock(std::time::Instant);
    impl TimeSource for AdvancingClock {
        fn now(&self) -> FrameworkTime {
            FrameworkTime::from_nanoseconds(self.0.elapsed().as_nanos() as i64)
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let (done, completed) = channel::bounded(1);
    let mut builder = GraphBuilder::new();
    let count = calls.clone();
    builder.add_scheduled_callback(
        "periodic",
        CallbackSchedule::periodic(Duration::from_millis(1)),
        move || {
            Ok(move |_| {
                if count.fetch_add(1, Ordering::SeqCst) == 2 {
                    done.send(()).unwrap();
                }
                Ok(())
            })
        },
    );
    let executor = LiveExecutor::new_multi_pool_with_time(
        vec![2],
        builder.build().unwrap(),
        AdvancingClock(std::time::Instant::now()),
    )
    .unwrap();
    executor
        .run_with(|stop| {
            wait(&completed);
            stop.request_stop();
        })
        .unwrap();
    assert!(calls.load(Ordering::SeqCst) >= 3);
}

#[test]
fn custom_deadline_uses_injected_clock_and_can_disable_its_timer() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let schedule = CallbackSchedule::default().with_next_execution_time_callback(move |now| {
        assert_eq!(now, FrameworkTime::from_nanoseconds(1234));
        (count.load(Ordering::Acquire) == 0).then_some(now)
    });
    let (done, completed) = channel::bounded(1);
    let count = calls.clone();
    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback("custom", schedule, move || {
        Ok(move |_| {
            count.fetch_add(1, Ordering::Release);
            done.send(()).unwrap();
            Ok(())
        })
    });
    let executor =
        LiveExecutor::new_multi_pool_with_time(vec![1], builder.build().unwrap(), Clock).unwrap();
    executor
        .run_with(|stop| {
            wait(&completed);
            stop.request_stop();
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

#[test]
fn controller_panic_joins_sleeping_workers_and_periodic_thread() {
    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback(
        "sleeping",
        CallbackSchedule::periodic(Duration::from_secs(3600)),
        || Ok(|_| Ok(())),
    );
    let executor =
        LiveExecutor::new_multi_pool_with_time(vec![3], builder.build().unwrap(), Clock).unwrap();
    let stop = executor.stop_signal();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        executor.run_with(|_| panic!("controller panic"))
    }));
    assert!(result.is_err());
    assert!(stop.is_stopped());
}

#[test]
fn stop_before_run_and_invalid_pool_configuration() {
    let mut builder = GraphBuilder::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    builder.add_scheduled_callback("source", CallbackSchedule::on_start(), move || {
        Ok(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    let executor =
        LiveExecutor::new_multi_pool_with_time(vec![2], builder.build().unwrap(), Clock).unwrap();
    executor.stop_signal().request_stop();
    executor.run().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(LiveExecutor::new(0, GraphBuilder::new().build().unwrap()).is_err());
    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback("bad pool", CallbackSchedule::on_start().in_pool(1), || {
        Ok(|_| Ok(()))
    });
    assert!(LiveExecutor::new(1, builder.build().unwrap()).is_err());
}
