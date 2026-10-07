use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use borrowed_task::time::FrameworkTime;
use borrowed_task::{
    ChannelEndpoints, ForwardedMessage, GraphPlan, GraphStorage, LoanError, PublisherStorage,
    PublisherStoragePlan, StorageError,
};

fn timestamp() -> FrameworkTime {
    FrameworkTime::from_nanoseconds(1)
}

type Forwarded<'storage> = ForwardedMessage<'storage, bool, u64>;
type ForwardingStorage<'storage> =
    GraphStorage<(PublisherStorage<Forwarded<'storage>>, PublisherStorage<u64>)>;

struct ForwardingGraph<'storage> {
    source: ChannelEndpoints<'storage, u64>,
    forwarding: ChannelEndpoints<'storage, Forwarded<'storage>>,
}

impl<'storage> ForwardingGraph<'storage> {
    fn build(storage: &'storage ForwardingStorage<'storage>) -> Self {
        Self {
            source: storage.channels().1.build(),
            forwarding: storage.channels().0.build(),
        }
    }

    fn publish(&mut self, value: u64) {
        self.source.publisher.publish(value).unwrap();
        self.source.publisher.flush(timestamp());
        self.source.subscribers[0].update();
        let message = self.source.subscribers[0].input().pop().unwrap();
        self.forwarding
            .publisher
            .publish(ForwardedMessage::new(true, message))
            .unwrap();
        self.forwarding.publisher.flush(timestamp());
        self.forwarding.subscribers[0].update();
    }
}

#[test]
fn source_and_forwarding_arenas_share_one_owner() {
    let forwarding = PublisherStoragePlan::new(1).with_subscriber(1);
    let source = PublisherStoragePlan::new(1)
        .with_subscriber(1)
        .with_retained_capacity(forwarding.capacity().unwrap());
    // Destination storage precedes source storage: no drop-order dependency.
    let storage = GraphPlan::new((forwarding, source)).allocate().unwrap();
    let mut graph = ForwardingGraph::build(&storage);
    graph.publish(42);
    let retained = graph.forwarding.subscribers[0].input().pop().unwrap();
    drop(graph);
    assert_eq!(retained.message.forwarded.message, 42);

    // Rebuilding endpoints neither invalidates nor resets retained messages.
    let mut rebuilt = ForwardingGraph::build(&storage);
    rebuilt.publish(100);
    let next = rebuilt.forwarding.subscribers[0].input().pop().unwrap();
    drop(rebuilt);
    assert_eq!(retained.message.forwarded.message, 42);
    assert_eq!(next.message.forwarded.message, 100);
    drop((retained, next));
}

#[test]
fn nested_forwarding_and_unrelated_channels_share_storage() {
    let last = PublisherStoragePlan::new(1).with_subscriber(1);
    let middle = PublisherStoragePlan::new(1)
        .with_subscriber(1)
        .with_retained_capacity(last.capacity().unwrap());
    let source = PublisherStoragePlan::new(1)
        .with_subscriber(1)
        .with_retained_capacity(middle.capacity().unwrap());
    let storage = GraphPlan::new((
        source,
        (
            middle,
            (
                last,
                PublisherStoragePlan::<String>::new(1).with_subscriber(1),
            ),
        ),
    ))
    .allocate()
    .unwrap();
    let mut source = storage.channels().0.build();
    let mut middle = storage.channels().1.0.build();
    let mut last = storage.channels().1.1.0.build();
    let mut strings = storage.channels().1.1.1.build();
    source.publisher.publish(42_u64).unwrap();
    source.publisher.flush(timestamp());
    source.subscribers[0].update();
    let input = source.subscribers[0].input().pop().unwrap();
    middle
        .publisher
        .publish(ForwardedMessage::new(true, input))
        .unwrap();
    middle.publisher.flush(timestamp());
    middle.subscribers[0].update();
    let input = middle.subscribers[0].input().pop().unwrap();
    last.publisher
        .publish(ForwardedMessage::new(7_u32, input))
        .unwrap();
    last.publisher.flush(timestamp());
    last.subscribers[0].update();
    let retained = last.subscribers[0].input().pop().unwrap();
    strings
        .publisher
        .publish(String::from("independent"))
        .unwrap();
    strings.publisher.flush(timestamp());
    strings.subscribers[0].update();
    assert_eq!(
        strings.subscribers[0].input().pop().unwrap().message,
        "independent"
    );
    drop((source, middle, last, strings));
    assert_eq!(retained.message.message, 7);
    assert!(retained.message.forwarded.message.message);
    assert_eq!(retained.message.forwarded.message.forwarded.message, 42);
}

#[test]
fn planned_capacity_covers_pending_and_both_subscriber_buffers() {
    let plan = PublisherStoragePlan::<u64>::new(2).with_subscriber(2);
    assert_eq!(plan.capacity().unwrap(), 7);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    assert_eq!(storage.channels().capacity(), 7);
    let mut graph = storage.channels().build();
    for value in [1, 2] {
        graph.publisher.publish(value).unwrap();
    }
    graph.publisher.flush(timestamp());
    graph.subscribers[0].update();
    for value in [3, 4] {
        graph.publisher.publish(value).unwrap();
    }
    graph.publisher.flush(timestamp());
    // Two in read, two in write, two pending: none may be reclaimed early.
    for value in [5, 6] {
        graph.publisher.publish(value).unwrap();
    }
    let old: Vec<_> = graph.subscribers[0]
        .input()
        .drain()
        .map(|p| p.message)
        .collect();
    assert_eq!(old, [1, 2]);
    graph.subscribers[0].update();
    let written: Vec<_> = graph.subscribers[0]
        .input()
        .drain()
        .map(|p| p.message)
        .collect();
    assert_eq!(written, [3, 4]);
    graph.publisher.flush(timestamp());
    graph.subscribers[0].update();
    let pending: Vec<_> = graph.subscribers[0]
        .input()
        .drain()
        .map(|p| p.message)
        .collect();
    assert_eq!(pending, [5, 6]);
}

#[test]
fn retained_messages_exhaust_budget_without_becoming_invalid() {
    let storage = GraphPlan::new(
        PublisherStoragePlan::<u64>::new(1)
            .with_subscriber(1)
            .with_retained_capacity(2),
    )
    .allocate()
    .unwrap();
    let mut graph = storage.channels().build();
    let mut retained = Vec::new();
    for value in 0..storage.channels().capacity() {
        graph.publisher.publish(value as u64).unwrap();
        graph.publisher.flush(timestamp());
        graph.subscribers[0].update();
        retained.push(graph.subscribers[0].input().pop().unwrap());
    }
    drop(graph);
    let mut graph = storage.channels().build();
    assert!(matches!(
        graph.publisher.loan_uninit(),
        Err(LoanError::ArenaExhausted)
    ));
    for (value, message) in retained.iter().enumerate() {
        assert_eq!(message.message, value as u64);
    }
    retained.pop();
    graph.publisher.publish(100).unwrap();
    graph.publisher.flush(timestamp());
    graph.subscribers[0].update();
    assert_eq!(graph.subscribers[0].input().pop().unwrap().message, 100);
}

#[test]
fn invalid_layouts_are_rejected_before_allocating_any_channel() {
    assert!(matches!(
        GraphPlan::new(PublisherStoragePlan::<u64>::new(1).with_subscriber(0)).allocate(),
        Err(StorageError::ZeroSubscriberCapacity)
    ));
    for plan in [
        PublisherStoragePlan::<u64>::new(usize::MAX).with_retained_capacity(1),
        PublisherStoragePlan::<u64>::new(1).with_subscriber(usize::MAX),
        PublisherStoragePlan::<u64>::new(usize::MAX).with_subscriber(1),
    ] {
        assert!(matches!(
            GraphPlan::new(plan).allocate(),
            Err(StorageError::CapacityOverflow)
        ));
    }
    // If allocation begins before whole-layout validation, this first channel
    // would attempt an impossible allocation instead of reporting the second's error.
    let invalid = (
        PublisherStoragePlan::<u64>::new(usize::MAX),
        PublisherStoragePlan::<u64>::new(1).with_subscriber(0),
    );
    assert!(matches!(
        GraphPlan::new(invalid).allocate(),
        Err(StorageError::ZeroSubscriberCapacity)
    ));
    let empty = GraphPlan::new(()).allocate().unwrap();
    assert_eq!(*empty.channels(), ());
}

struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn graph_construction_unwind_releases_loans_and_allows_rebuild() {
    let drops = Arc::new(AtomicUsize::new(0));
    let storage = GraphPlan::new((
        PublisherStoragePlan::<Counted>::new(1),
        PublisherStoragePlan::<u64>::new(1),
    ))
    .allocate()
    .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut partially_built = storage.channels().0.build();
        partially_built
            .publisher
            .publish(Counted(drops.clone()))
            .unwrap();
        panic!("another node failed during graph construction");
    }));
    assert!(result.is_err());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    let mut rebuilt = storage.channels().0.build();
    rebuilt.publisher.publish(Counted(drops.clone())).unwrap();
    drop(rebuilt);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}
