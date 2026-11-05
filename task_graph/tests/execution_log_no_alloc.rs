//! Execution logging must not allocate on the worker thread.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::BTreeMap,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use task::{
    Callback, ChannelPlan, Context, LoanError, SubscriberPolicy,
    execution_log_schema::{
        Direction, EXECUTION_LOG_CHANNEL, EndpointDescriptor, ExecutionLogMode, ExecutionLogModes,
        ExecutionRecord, IOX2_EVENT_RECORD_CHANNEL, Iox2EventRecord, LoggedMessage, Outcome,
        Transport,
    },
    message::MessageHeader,
    time::FrameworkTime,
};
use task_graph::{GraphBuilder, execution_log::ExecutionLogPublishers};

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

// SAFETY: every method forwards the same arguments to the system allocator and
// only observes a counter around the call.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded unchanged from the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
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

struct Recorded {
    fail: bool,
    input: LoggedMessage,
    output: LoggedMessage,
}

fn message(ordinal: usize, at: i64) -> LoggedMessage {
    LoggedMessage {
        ordinal,
        header: MessageHeader::new(FrameworkTime::from_nanos(at)),
    }
}

impl Recorded {
    fn success() -> Self {
        Self {
            fail: false,
            input: message(0, 1),
            output: message(0, 1),
        }
    }

    fn failure() -> Self {
        Self {
            fail: true,
            ..Self::success()
        }
    }
}

impl Callback for Recorded {
    fn run(&mut self, _context: &Context) -> Result<(), LoanError> {
        if self.fail {
            Err(LoanError::PendingBatchNotEmpty)
        } else {
            Ok(())
        }
    }

    fn recording_endpoints(&self) -> Option<Vec<EndpointDescriptor>> {
        Some(vec![
            EndpointDescriptor {
                ordinal: 0,
                channel: "input".into(),
                direction: Direction::Received,
                transport: Transport::Native,
                payload_type: "u64".into(),
                publisher_index: None,
            },
            EndpointDescriptor {
                ordinal: 0,
                channel: "output".into(),
                direction: Direction::Published,
                transport: Transport::Native,
                payload_type: "u64".into(),
                publisher_index: Some(0),
            },
        ])
    }

    fn visit_prepared_messages(&self, visit: &mut dyn FnMut(LoggedMessage)) {
        visit(self.input);
    }

    fn visit_pending_messages(&self, visit: &mut dyn FnMut(LoggedMessage)) {
        visit(self.output);
    }

    fn required_inputs_ready(&self) -> bool {
        true
    }
}

#[test]
fn execution_recording_is_allocation_free() {
    let policy = SubscriberPolicy {
        trigger: false,
        keep_across_runs: true,
    };
    let mut record_plan = ChannelPlan::<ExecutionRecord>::new(EXECUTION_LOG_CHANNEL);
    let ok_record_publisher = record_plan.publisher(1);
    let fail_record_publisher = record_plan.publisher(1);
    let record_subscriber_key = record_plan.subscriber_with_policy(8, policy);
    let mut event_plan = ChannelPlan::<Iox2EventRecord>::new(IOX2_EVENT_RECORD_CHANNEL);
    let ok_event_publisher = event_plan.publisher(1);
    let fail_event_publisher = event_plan.publisher(1);
    let event_subscriber_key = event_plan.subscriber_with_policy(8, policy);

    let record_storage = record_plan.allocate().unwrap();
    let event_storage = event_plan.allocate().unwrap();
    let record_bindings = record_storage.bind();
    let event_bindings = event_storage.bind();
    let records = record_bindings
        .take_subscriber(&record_subscriber_key)
        .unwrap();
    let _events = event_bindings
        .take_subscriber(&event_subscriber_key)
        .unwrap();

    let mut graph = GraphBuilder::new();
    graph.add_callback("ok", || Ok(Recorded::success()));
    graph.add_callback("fails", || Ok(Recorded::failure()));
    let graph = graph.build().unwrap();
    let (graph, _) = graph
        .with_execution_log(
            ExecutionLogModes::new(ExecutionLogMode::Full),
            ExecutionLogPublishers {
                records: BTreeMap::from([
                    (
                        "ok".into(),
                        record_bindings
                            .take_publisher(&ok_record_publisher)
                            .unwrap(),
                    ),
                    (
                        "fails".into(),
                        record_bindings
                            .take_publisher(&fail_record_publisher)
                            .unwrap(),
                    ),
                ]),
                iox2_events: BTreeMap::from([
                    (
                        "ok".into(),
                        event_bindings.take_publisher(&ok_event_publisher).unwrap(),
                    ),
                    (
                        "fails".into(),
                        event_bindings
                            .take_publisher(&fail_event_publisher)
                            .unwrap(),
                    ),
                ]),
            },
        )
        .unwrap();
    let (mut nodes, metadata) = graph.into_parts();
    let context = metadata.context(FrameworkTime::from_nanos(1));

    ALLOCATIONS.store(0, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    let success = task::execute_callback(nodes[0].callback.as_mut(), &context);
    let failure = task::execute_callback(nodes[1].callback.as_mut(), &context);
    ARMED.store(false, Ordering::Relaxed);

    assert!(success.unwrap());
    assert!(failure.is_err());
    assert_eq!(
        ALLOCATIONS.load(Ordering::Relaxed),
        0,
        "execution logging allocated on the worker thread"
    );

    records.update();
    let published: Vec<ExecutionRecord> = records
        .input_forwardable()
        .drain()
        .map(|message| message.message.clone())
        .collect();
    assert_eq!(published.len(), 2);
    assert_eq!(published[0].outcome, Outcome::Committed);
    assert_eq!(published[0].inputs.len(), 1);
    assert_eq!(published[0].outputs.len(), 1);
    assert_eq!(published[0].truncated, 0);
    assert_eq!(
        published[1].outcome,
        Outcome::BodyError(LoanError::PendingBatchNotEmpty)
    );
    assert!(published[1].outputs.is_empty());
    assert_eq!(published[1].truncated, 0);
}
