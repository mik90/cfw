use simulation_executor::{SimulationConfig, SimulationState, StepError};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use task::{CallbackSchedule, ChannelPlan, GraphBuilder, GraphPlan, LoanError, Publisher};
use task_macros::task_callback;

struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Sender {
    drops: Arc<AtomicUsize>,
    fail: u8,
}
#[task_callback]
impl Sender {
    fn run(&self, output: &mut Publisher<Counted>) -> Result<(), LoanError> {
        output.publish(Counted(self.drops.clone()))?;
        assert!(self.fail != 2, "callback panicked");
        if self.fail == 1 {
            return Err(LoanError::LoanCapacityReached);
        }
        Ok(())
    }
}

#[test]
fn body_error_panic_and_post_execution_timing_failure_cancel_the_whole_batch() {
    for fail in [1, 2, 3] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut plan = ChannelPlan::new("output");
        let first = Sender::declare(&mut plan).unwrap();
        let second = Sender::declare(&mut plan).unwrap();
        let capture_key = plan.subscriber(2);
        let storage = GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build();
        let capture = bindings.take_subscriber(&capture_key).unwrap();
        let mut builder = GraphBuilder::new();
        builder.add_scheduled_callback("first", CallbackSchedule::on_start(), || {
            Ok(Sender {
                drops: drops.clone(),
                fail: 0,
            }
            .bind(first, &bindings)?)
        });
        let schedule = if fail == 3 {
            CallbackSchedule::on_start().with_execution_duration(Duration::MAX)
        } else {
            CallbackSchedule::on_start()
        };
        builder.add_scheduled_callback("second", schedule, || {
            Ok(Sender {
                drops: drops.clone(),
                fail,
            }
            .bind(second, &bindings)?)
        });
        let mut simulation = SimulationState::with_config(
            builder.build().unwrap(),
            SimulationConfig {
                virtual_pool_threads: vec![2],
                node_executor_thread_count: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(simulation.step().is_err());
        assert!(matches!(simulation.step(), Err(StepError::Poisoned)));
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        capture.update();
        assert!(capture.input().pop().is_none());
        drop(simulation);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn panic_during_commit_preserves_already_published_messages_and_cancels_remaining_loans() {
    struct CommitPanic<'a>(Publisher<'a, Counted>, Arc<AtomicUsize>);
    impl task::Callback for CommitPanic<'_> {
        fn run(&mut self, _: &task::Context) -> Result<(), LoanError> {
            self.0.publish(Counted(self.1.clone()))
        }
        fn flush_outputs(&mut self, _: task::time::FrameworkTime) {
            panic!("flush failed");
        }
        fn discard_outputs(&mut self) {
            self.0.discard_pending();
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let mut plan = ChannelPlan::new("output");
    let first = Sender::declare(&mut plan).unwrap();
    let second = plan.publisher(1);
    let capture_key = plan.subscriber(2);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let capture = bindings.take_subscriber(&capture_key).unwrap();
    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback("first", CallbackSchedule::on_start(), || {
        Ok(Sender {
            drops: drops.clone(),
            fail: 0,
        }
        .bind(first, &bindings)?)
    });
    builder.add_scheduled_callback("second", CallbackSchedule::on_start(), || {
        Ok(CommitPanic(
            bindings.take_publisher(&second)?,
            drops.clone(),
        ))
    });
    let mut simulation = SimulationState::with_config(
        builder.build().unwrap(),
        SimulationConfig {
            virtual_pool_threads: vec![2],
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(simulation.step(), Err(StepError::Panicked)));
    drop(simulation);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    capture.update();
    let retained = capture.input().pop().unwrap();
    drop(capture);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(retained);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}
