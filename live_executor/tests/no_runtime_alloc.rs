//! Live execution must not allocate once the executor is running.
use crossbeam::channel::{self, Receiver, Sender};
use live_executor::LiveExecutor;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;
use task::executor::TimeSource;
use task::time::FrameworkTime;
use task::{ChannelPlan, OutputDefault, RequiredInput};
use task_graph::{CallbackSchedule, GraphBuilder, ThreadPoolConfigList};
use task_macros::task_callback;

const PERIOD: Duration = Duration::from_millis(1);
/// A few odd things allocate on first run
/// - Thread name is a Box<str> evidently and is set on first run
/// - crossbeam select allocates some context on first run
/// - crossbeam wakers dynamically allocates upon the first blocking recv
const WARMUP: usize = 1;
const AUDITED: usize = 8;

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

// SAFETY: every method forwards the same arguments to the system allocator and
// only observes a counter around the allocation calls.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded unchanged from the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged from the caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded unchanged from the caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded unchanged from the caller.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

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

/// Wait until the scheduler has no queued/running callbacks. This ensures the
/// producer recorded its next deadline before the clock advances again.
fn wait_idle(stop: &live_executor::StopSignal<'_>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !stop.is_idle() {
        assert!(
            std::time::Instant::now() < deadline,
            "executor did not become idle"
        );
        std::thread::yield_now();
    }
}

struct Produce {
    sent: usize,
    published: Sender<()>,
}

#[task_callback]
impl Produce {
    fn run(&mut self, mut output: OutputDefault<u64>) {
        if self.sent == WARMUP + AUDITED {
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
fn runtime_execution_does_not_allocate() {
    let mut plan = ChannelPlan::<u64>::new("no_runtime_alloc_input");
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
    let executor = LiveExecutor::new_multi_pool_with_time(
        ThreadPoolConfigList::try_from([2]).unwrap(),
        builder.build().unwrap(),
        clock.clone(),
    )
    .unwrap();

    executor
        .run_with(move |stop| {
            // Warm up lazily initialized per-thread machinery (crossbeam's select
            // context in particular) before arming the allocation counter.
            clock.0.store(PERIOD.as_nanos() as i64, Ordering::Release);
            wait(&published_events);
            wait(&consumed_events);
            wait_idle(stop);

            // Force crossbeam to register the controller's waiters before the
            // audited window; registration grows the channel's waker vector.
            assert!(matches!(
                published_events.recv_timeout(Duration::from_millis(10)),
                Err(channel::RecvTimeoutError::Timeout)
            ));
            assert!(matches!(
                consumed_events.recv_timeout(Duration::from_millis(10)),
                Err(channel::RecvTimeoutError::Timeout)
            ));

            let before = ALLOCATIONS.load(Ordering::SeqCst);
            ARMED.store(true, Ordering::Release);

            for step in 2..=(WARMUP + AUDITED) as i64 {
                clock
                    .0
                    .store(step * PERIOD.as_nanos() as i64, Ordering::Release);
                wait(&published_events);
                wait(&consumed_events);
                wait_idle(stop);
            }

            ARMED.store(false, Ordering::Release);

            assert_eq!(
                ALLOCATIONS.load(Ordering::SeqCst),
                before,
                "live execution allocated on the heap"
            );
        })
        .unwrap();
}
