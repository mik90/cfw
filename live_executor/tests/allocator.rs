//! The executor's owned storage must come from the caller's allocator.
use crossbeam::channel::{self, Receiver, Sender};
use live_executor::LiveExecutor;
use std::alloc::{AllocError, Allocator, Global, Layout};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;
use task::executor::TimeSource;
use task::time::FrameworkTime;
use task::{ChannelPlan, OutputDefault, RequiredInput};
use task_graph::{CallbackSchedule, GraphBuilder, ThreadPoolConfigList};
use task_macros::task_callback;

const PERIOD: Duration = Duration::from_millis(1);
const LIMIT: usize = 8;

#[derive(Clone, Default)]
struct CountingAllocator {
    allocations: Arc<AtomicUsize>,
    deallocations: Arc<AtomicUsize>,
}

// SAFETY: every method forwards the same arguments to the global allocator and
// only observes a counter around the call.
unsafe impl Allocator for CountingAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.allocations.fetch_add(1, Ordering::SeqCst);
        Global.allocate(layout)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        self.deallocations.fetch_add(1, Ordering::SeqCst);
        // SAFETY: forwarded unchanged from the caller.
        unsafe { Global.deallocate(ptr, layout) }
    }
}

#[derive(Clone)]
struct TestClock(Arc<AtomicI64>);
impl TimeSource for TestClock {
    fn now(&self) -> FrameworkTime {
        FrameworkTime::from_nanos(self.0.load(Ordering::Acquire))
    }
}

fn wait<T>(receiver: &Receiver<T>) -> T {
    receiver
        .recv_timeout(Duration::from_secs(20))
        .expect("worker did not respond")
}

struct Produce {
    sent: usize,
    published: Sender<()>,
}

#[task_callback]
impl Produce {
    fn run(&mut self, mut output: OutputDefault<u64>) {
        if self.sent == LIMIT {
            return;
        }
        self.sent += 1;
        *output = self.sent as u64;
        output.send();
        self.published.send(()).unwrap();
    }
}

struct Consume {
    consumed: Sender<()>,
}

#[task_callback]
impl Consume {
    fn run(&mut self, #[trigger(true)] mut input: RequiredInput<u64>) {
        input.clear();
        self.consumed.send(()).unwrap();
    }
}

#[test]
fn executor_storage_is_allocated_from_the_supplied_allocator() {
    let mut plan = ChannelPlan::<u64>::new("allocator_test_input");
    let produce = Produce::declare(&mut plan).unwrap();
    let consume = Consume::declare(&mut plan).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind();

    let (published, published_events) = channel::bounded(1);
    let (consumed, consumed_events) = channel::bounded(1);

    let mut schedule = CallbackSchedule::periodic(PERIOD);
    schedule.run_on_start = false;

    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback("produce", schedule, || {
        Ok(Produce { sent: 0, published }.bind(produce, &bindings)?)
    });
    builder.add_callback("consume", || {
        Ok(Consume { consumed }.bind(consume, &bindings)?)
    });

    let clock = TestClock(Arc::new(AtomicI64::new(0)));
    let allocator = CountingAllocator::default();
    let allocations = allocator.allocations.clone();
    let deallocations = allocator.deallocations.clone();

    let executor = LiveExecutor::new_multi_pool_with_time_in(
        ThreadPoolConfigList::try_from([2]).unwrap(),
        builder.build().unwrap(),
        clock.clone(),
        allocator,
    )
    .unwrap();

    assert!(
        allocations.load(Ordering::SeqCst) > 0,
        "executor storage did not use the supplied allocator"
    );

    let runtime_allocations = allocations.clone();
    executor
        .run_with(move |stop| {
            let before = runtime_allocations.load(Ordering::SeqCst);

            // Drive one periodic producer and input-triggered consumer pass at a
            // time so every executor thread runs inside the audited window.
            for step in 1..=LIMIT as i64 {
                clock
                    .0
                    .store(step * PERIOD.as_nanos() as i64, Ordering::Release);
                wait(&published_events);
                wait(&consumed_events);
            }

            assert!(stop.drain(clock.now(), Duration::from_secs(20)));
            assert_eq!(
                runtime_allocations.load(Ordering::SeqCst),
                before,
                "allocator was used after the executor started"
            );
        })
        .unwrap();

    assert_eq!(
        deallocations.load(Ordering::SeqCst),
        allocations.load(Ordering::SeqCst),
        "executor storage leaked allocations from the supplied allocator"
    );
}
