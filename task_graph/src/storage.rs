//! Typed multi-channel storage composition.
use std::{collections::HashSet, sync::Arc};
use task::{StorageError, StorageLayout, string_interner::ChannelNameInterner};

/// A typed layout of channels validated together before allocation.
pub struct StoragePlan<L> {
    layout: L,
}

impl<L: StorageLayout> StoragePlan<L> {
    pub fn new(layout: L) -> Self {
        Self { layout }
    }

    pub fn allocate(self) -> Result<Storage<L::Storage>, StorageError> {
        self.layout.validate()?;
        let mut names = HashSet::new();
        self.layout.validate_channel_names(&mut names)?;
        let mut names: Vec<_> = names.into_iter().collect();
        names.sort();
        let mut channel_names = ChannelNameInterner::new();
        for name in names {
            channel_names.intern(&name);
        }
        channel_names.shrink_to_fit();
        Ok(Storage {
            channels: self.layout.allocate()?,
            channel_names: Arc::new(channel_names),
        })
    }
}

/// External owner of typed channel arenas. Endpoints and retained messages
/// borrow this owner; last arena handles destroy initialized payloads.
///
/// ```
/// use task::ChannelPlan;
/// use task_graph::StoragePlan;
/// let mut channel = ChannelPlan::<u64>::new("counter");
/// let key = channel.publisher(1);
/// let storage = StoragePlan::new(channel).allocate().unwrap();
/// let bindings = storage.channels().build();
/// let publisher = bindings.take_publisher(&key).unwrap();
/// ```
///
/// ```compile_fail,E0505
/// use task::PublisherStoragePlan;
/// use task_graph::StoragePlan;
/// use task::time::FrameworkTime;
/// let storage = StoragePlan::new(PublisherStoragePlan::<u64>::new(1).with_subscriber(1))
///     .allocate().unwrap();
/// let mut endpoints = storage.channels().build();
/// endpoints.publisher.publish(42).unwrap();
/// endpoints.publisher.flush(FrameworkTime::from_nanoseconds(1));
/// endpoints.subscribers[0].update();
/// let message = endpoints.subscribers[0].input_forwardable().pop().unwrap();
/// drop(endpoints);
/// drop(storage);
/// assert_eq!(message.message, 42);
/// ```
pub struct Storage<C> {
    channels: C,
    channel_names: Arc<ChannelNameInterner>,
}

impl<C> Storage<C> {
    pub fn channel_names(&self) -> &Arc<ChannelNameInterner> {
        &self.channel_names
    }
    pub fn channels(&self) -> &C {
        &self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::{Storage, StoragePlan};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use task::time::FrameworkTime;
    use task::{ChannelEndpoints, LoanError, PublisherStorage, PublisherStoragePlan, StorageError};

    fn timestamp() -> FrameworkTime {
        FrameworkTime::from_nanoseconds(1)
    }

    type Forwarded<'storage> =
        task::Forwarded<bool, task::ArenaReaderPtr<'storage, task::message::Message<u64>>>;
    type ForwardingStorage<'storage> =
        Storage<(PublisherStorage<Forwarded<'storage>>, PublisherStorage<u64>)>;

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
            let message = self.source.subscribers[0]
                .input_forwardable()
                .pop()
                .unwrap();
            self.forwarding
                .publisher
                .publish(task::Forwarded::new(true, message))
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
        let storage = StoragePlan::new((forwarding, source)).allocate().unwrap();
        let mut graph = ForwardingGraph::build(&storage);
        graph.publish(42);
        let retained = graph.forwarding.subscribers[0]
            .input_forwardable()
            .pop()
            .unwrap();
        drop(graph);
        assert_eq!(retained.message.forwarded_messages.message, 42);

        // Rebuilding endpoints neither invalidates nor resets retained messages.
        let mut rebuilt = ForwardingGraph::build(&storage);
        rebuilt.publish(100);
        let next = rebuilt.forwarding.subscribers[0]
            .input_forwardable()
            .pop()
            .unwrap();
        drop(rebuilt);
        assert_eq!(retained.message.forwarded_messages.message, 42);
        assert_eq!(next.message.forwarded_messages.message, 100);
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
        let storage = StoragePlan::new((
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
        let input = source.subscribers[0].input_forwardable().pop().unwrap();
        middle
            .publisher
            .publish(task::Forwarded::new(true, input))
            .unwrap();
        middle.publisher.flush(timestamp());
        middle.subscribers[0].update();
        let input = middle.subscribers[0].input_forwardable().pop().unwrap();
        last.publisher
            .publish(task::Forwarded::new(7_u32, input))
            .unwrap();
        last.publisher.flush(timestamp());
        last.subscribers[0].update();
        let retained = last.subscribers[0].input_forwardable().pop().unwrap();
        strings
            .publisher
            .publish(String::from("independent"))
            .unwrap();
        strings.publisher.flush(timestamp());
        strings.subscribers[0].update();
        assert_eq!(
            strings.subscribers[0]
                .input_forwardable()
                .pop()
                .unwrap()
                .message,
            "independent"
        );
        drop((source, middle, last, strings));
        assert_eq!(retained.message.message, 7);
        assert!(retained.message.forwarded_messages.message.message);
        assert_eq!(
            retained
                .message
                .forwarded_messages
                .message
                .forwarded_messages
                .message,
            42
        );
    }

    #[test]
    fn planned_capacity_covers_pending_and_both_subscriber_buffers() {
        let plan = PublisherStoragePlan::<u64>::new(2).with_subscriber(2);
        assert_eq!(plan.capacity().unwrap(), 7);
        let storage = StoragePlan::new(plan).allocate().unwrap();
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
            .input_forwardable()
            .drain()
            .map(|p| p.message)
            .collect();
        assert_eq!(old, [1, 2]);
        graph.subscribers[0].update();
        let written: Vec<_> = graph.subscribers[0]
            .input_forwardable()
            .drain()
            .map(|p| p.message)
            .collect();
        assert_eq!(written, [3, 4]);
        graph.publisher.flush(timestamp());
        graph.subscribers[0].update();
        let pending: Vec<_> = graph.subscribers[0]
            .input_forwardable()
            .drain()
            .map(|p| p.message)
            .collect();
        assert_eq!(pending, [5, 6]);
    }

    #[test]
    fn retained_messages_exhaust_budget_without_becoming_invalid() {
        let storage = StoragePlan::new(
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
            retained.push(graph.subscribers[0].input_forwardable().pop().unwrap());
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
        assert_eq!(
            graph.subscribers[0]
                .input_forwardable()
                .pop()
                .unwrap()
                .message,
            100
        );
    }

    #[test]
    fn invalid_layouts_are_rejected_before_allocating_any_channel() {
        assert!(matches!(
            StoragePlan::new(PublisherStoragePlan::<u64>::new(1).with_subscriber(0)).allocate(),
            Err(StorageError::ZeroSubscriberCapacity)
        ));
        for plan in [
            PublisherStoragePlan::<u64>::new(usize::MAX).with_retained_capacity(1),
            PublisherStoragePlan::<u64>::new(1).with_subscriber(usize::MAX),
            PublisherStoragePlan::<u64>::new(usize::MAX).with_subscriber(1),
        ] {
            assert!(matches!(
                StoragePlan::new(plan).allocate(),
                Err(StorageError::CapacityOverflow)
            ));
        }
        // If allocation begins before whole-layout validation, this first channel
        // would attempt an impossible allocation instead of reporting the second's error.
        let invalid = (
            PublisherStoragePlan::<u64>::new(u32::MAX as usize),
            PublisherStoragePlan::<u64>::new(1).with_subscriber(0),
        );
        assert!(matches!(
            StoragePlan::new(invalid).allocate(),
            Err(StorageError::ZeroSubscriberCapacity)
        ));
        let empty = StoragePlan::new(()).allocate().unwrap();
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
        let storage = StoragePlan::new((
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
}
