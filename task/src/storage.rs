use std::marker::PhantomData;

use base::arena::Arena;

use super::Publisher;
use crate::message::Message;

/// Capacity errors are detected before allocating any arena in a graph layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    ZeroSubscriberCapacity,
    CapacityOverflow,
    ForeignPublisherKey,
    InvalidPublisherIndex(usize),
    InvalidConnection(String),
    Transport(String),
}

/// Storage requirements for one publisher and its subscribers.
///
/// `T` can contain borrowed forwarded messages; no `Any` or `'static` bound is
/// required. Planning neither allocates arenas nor borrows their storage.
pub(crate) struct PublisherStoragePlan<T> {
    loan_capacity: usize,
    subscribers: Vec<usize>,
    retained_capacity: usize,
    payload: PhantomData<fn() -> T>,
}

impl<T> PublisherStoragePlan<T> {
    pub(crate) fn new(loan_capacity: usize) -> Self {
        Self {
            loan_capacity,
            subscribers: Vec::new(),
            retained_capacity: 0,
            payload: PhantomData,
        }
    }

    /// Add a subscriber, preserving its position in the built endpoint list.
    pub(crate) fn with_subscriber(mut self, capacity: usize) -> Self {
        self.subscribers.push(capacity);
        self
    }

    /// Set the additional budget for messages retained outside these queues.
    ///
    /// For forwarding, each downstream arena slot can retain a source message.
    /// Planning downstream capacities first provides a conservative source budget.
    /// Additional user-held pointers also consume this budget. Exceeding it yields
    /// `LoanError::ArenaExhausted`, never permission to reclaim a live message.
    pub(crate) fn with_retained_capacity(mut self, capacity: usize) -> Self {
        self.retained_capacity = capacity;
        self
    }

    /// Pending loans + retained messages + each subscriber's read/write queues
    /// and one pointer in flight while transferring between them.
    pub(crate) fn capacity(&self) -> Result<usize, StorageError> {
        u32::try_from(self.loan_capacity).map_err(|_| StorageError::CapacityOverflow)?;
        let mut total = self
            .loan_capacity
            .checked_add(self.retained_capacity)
            .ok_or(StorageError::CapacityOverflow)?;
        for &capacity in &self.subscribers {
            if capacity == 0 {
                return Err(StorageError::ZeroSubscriberCapacity);
            }
            let footprint = capacity
                .checked_mul(2)
                .and_then(|n| n.checked_add(1))
                .ok_or(StorageError::CapacityOverflow)?;
            total = total
                .checked_add(footprint)
                .ok_or(StorageError::CapacityOverflow)?;
        }
        Ok(total)
    }
}

/// Frozen per-publisher allocation. All builds share this publisher's fixed budget.
/// A fresh build does not reset or reclaim messages retained from a prior build.
/// Retained messages borrow their storage, even after endpoints are dropped.
#[derive(Debug)]
pub(crate) struct PublisherStorage<MessageType> {
    arena: Arena<Message<MessageType>>,
    loan_capacity: usize,
}

impl<MessageType> PublisherStorage<MessageType> {
    pub(crate) fn publisher(&self) -> Publisher<'_, MessageType> {
        Publisher::new(self.arena.allocator(), self.loan_capacity)
    }

    pub(crate) fn capacity(&self) -> usize {
        self.arena.capacity()
    }
}

impl<T> PublisherStoragePlan<T> {
    /// Validate the capacity and allocate this publisher's arena.
    pub(crate) fn allocate(self) -> Result<PublisherStorage<T>, StorageError> {
        Ok(PublisherStorage {
            arena: Arena::new(self.capacity()?),
            loan_capacity: self.loan_capacity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelPlan, Forwarded, LoanError, time::FrameworkTime};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn timestamp() -> FrameworkTime {
        FrameworkTime::from_nanoseconds(1)
    }

    #[test]
    fn forwarding_retains_source_messages_across_endpoint_rebuilds() {
        let mut forwarding_plan = ChannelPlan::new("forwarding");
        let forwarding_publisher_key = forwarding_plan.publisher(1);
        let forwarding_subscriber_key = forwarding_plan.subscriber(1);
        let mut source_plan = ChannelPlan::new("source");
        let source_publisher_key = source_plan.publisher(1);
        let source_subscriber_key = source_plan.subscriber(1);
        source_plan
            .reserve_retained(
                &source_publisher_key,
                forwarding_plan
                    .publisher_capacity(&forwarding_publisher_key)
                    .unwrap(),
            )
            .unwrap();
        let source = source_plan.allocate().unwrap();
        let forwarding = forwarding_plan.allocate().unwrap();
        let mut retained = Vec::new();
        for value in [42_u64, 100] {
            let source_bindings = source.build();
            let forwarding_bindings = forwarding.build();
            let mut source_publisher = source_bindings
                .take_publisher(&source_publisher_key)
                .unwrap();
            let source_subscriber = source_bindings
                .take_subscriber(&source_subscriber_key)
                .unwrap();
            let mut forwarding_publisher = forwarding_bindings
                .take_publisher(&forwarding_publisher_key)
                .unwrap();
            let forwarding_subscriber = forwarding_bindings
                .take_subscriber(&forwarding_subscriber_key)
                .unwrap();
            source_publisher.publish(value).unwrap();
            source_publisher.flush(timestamp());
            source_subscriber.update();
            let message = source_subscriber.input_forwardable().pop().unwrap();
            forwarding_publisher
                .publish(Forwarded::new(true, message))
                .unwrap();
            forwarding_publisher.flush(timestamp());
            forwarding_subscriber.update();
            retained.push(forwarding_subscriber.input_forwardable().pop().unwrap());
        }
        assert_eq!(retained[0].message.forwarded_messages.message, 42);
        assert_eq!(retained[1].message.forwarded_messages.message, 100);
    }

    #[test]
    fn nested_forwarding_retains_original_source_storage() {
        let mut last_plan = ChannelPlan::new("last");
        let last_publisher_key = last_plan.publisher(1);
        let last_subscriber_key = last_plan.subscriber(1);
        let mut middle_plan = ChannelPlan::new("middle");
        let middle_publisher_key = middle_plan.publisher(1);
        let middle_subscriber_key = middle_plan.subscriber(1);
        middle_plan
            .reserve_retained(
                &middle_publisher_key,
                last_plan.publisher_capacity(&last_publisher_key).unwrap(),
            )
            .unwrap();
        let mut source_plan = ChannelPlan::new("source");
        let source_publisher_key = source_plan.publisher(1);
        let source_subscriber_key = source_plan.subscriber(1);
        source_plan
            .reserve_retained(
                &source_publisher_key,
                middle_plan
                    .publisher_capacity(&middle_publisher_key)
                    .unwrap(),
            )
            .unwrap();
        let source_storage = source_plan.allocate().unwrap();
        let middle_storage = middle_plan.allocate().unwrap();
        let last_storage = last_plan.allocate().unwrap();
        let source_bindings = source_storage.build();
        let middle_bindings = middle_storage.build();
        let last_bindings = last_storage.build();
        let mut source_publisher = source_bindings
            .take_publisher(&source_publisher_key)
            .unwrap();
        let source_subscriber = source_bindings
            .take_subscriber(&source_subscriber_key)
            .unwrap();
        let mut middle_publisher = middle_bindings
            .take_publisher(&middle_publisher_key)
            .unwrap();
        let middle_subscriber = middle_bindings
            .take_subscriber(&middle_subscriber_key)
            .unwrap();
        let mut last_publisher = last_bindings.take_publisher(&last_publisher_key).unwrap();
        let last_subscriber = last_bindings.take_subscriber(&last_subscriber_key).unwrap();
        source_publisher.publish(42_u64).unwrap();
        source_publisher.flush(timestamp());
        source_subscriber.update();
        let input = source_subscriber.input_forwardable().pop().unwrap();
        middle_publisher
            .publish(Forwarded::new(true, input))
            .unwrap();
        middle_publisher.flush(timestamp());
        middle_subscriber.update();
        let input = middle_subscriber.input_forwardable().pop().unwrap();
        last_publisher
            .publish(Forwarded::new(7_u32, input))
            .unwrap();
        last_publisher.flush(timestamp());
        last_subscriber.update();
        let retained = last_subscriber.input_forwardable().pop().unwrap();
        drop((source_publisher, source_subscriber, source_bindings));
        drop((middle_publisher, middle_subscriber, middle_bindings));
        drop((last_publisher, last_subscriber, last_bindings));
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
        let mut plan = ChannelPlan::<u64>::new("numbers");
        let publisher_key = plan.publisher(2);
        let subscriber_key = plan.subscriber(2);
        assert_eq!(plan.publisher_capacity(&publisher_key).unwrap(), 7);
        let storage = plan.allocate().unwrap();
        assert_eq!(storage.publisher_capacity(&publisher_key).unwrap(), 7);
        let bindings = storage.build();
        let mut publisher = bindings.take_publisher(&publisher_key).unwrap();
        let subscriber = bindings.take_subscriber(&subscriber_key).unwrap();
        for value in [1, 2] {
            publisher.publish(value).unwrap();
        }
        publisher.flush(timestamp());
        subscriber.update();
        for value in [3, 4] {
            publisher.publish(value).unwrap();
        }
        publisher.flush(timestamp());
        for value in [5, 6] {
            publisher.publish(value).unwrap();
        }
        let old: Vec<_> = subscriber
            .input_forwardable()
            .drain()
            .map(|p| p.message)
            .collect();
        assert_eq!(old, [1, 2]);
        subscriber.update();
        let written: Vec<_> = subscriber
            .input_forwardable()
            .drain()
            .map(|p| p.message)
            .collect();
        assert_eq!(written, [3, 4]);
        publisher.flush(timestamp());
        subscriber.update();
        let pending: Vec<_> = subscriber
            .input_forwardable()
            .drain()
            .map(|p| p.message)
            .collect();
        assert_eq!(pending, [5, 6]);
    }

    #[test]
    fn retained_messages_exhaust_budget_without_becoming_invalid() {
        let mut plan = ChannelPlan::<u64>::new("numbers");
        let publisher_key = plan.publisher(1);
        let subscriber_key = plan.subscriber(1);
        plan.reserve_retained(&publisher_key, 2).unwrap();
        let storage = plan.allocate().unwrap();
        let capacity = storage.publisher_capacity(&publisher_key).unwrap();
        let bindings = storage.build();
        let mut publisher = bindings.take_publisher(&publisher_key).unwrap();
        let subscriber = bindings.take_subscriber(&subscriber_key).unwrap();
        let mut retained = Vec::new();
        for value in 0..capacity {
            publisher.publish(value as u64).unwrap();
            publisher.flush(timestamp());
            subscriber.update();
            retained.push(subscriber.input_forwardable().pop().unwrap());
        }
        drop((publisher, subscriber, bindings));
        let bindings = storage.build();
        let mut publisher = bindings.take_publisher(&publisher_key).unwrap();
        assert!(matches!(
            publisher.loan_uninit(),
            Err(LoanError::ArenaExhausted)
        ));
        for (value, message) in retained.iter().enumerate() {
            assert_eq!(message.message, value as u64);
        }
        retained.pop();
        publisher.publish(100).unwrap();
        publisher.flush(timestamp());
        let subscriber = bindings.take_subscriber(&subscriber_key).unwrap();
        subscriber.update();
        assert_eq!(subscriber.input_forwardable().pop().unwrap().message, 100);
    }

    #[test]
    fn invalid_capacity_is_rejected_before_allocation() {
        assert!(matches!(
            PublisherStoragePlan::<u64>::new(1)
                .with_subscriber(0)
                .allocate(),
            Err(StorageError::ZeroSubscriberCapacity)
        ));
        for plan in [
            PublisherStoragePlan::<u64>::new(usize::MAX).with_retained_capacity(1),
            PublisherStoragePlan::<u64>::new(1).with_subscriber(usize::MAX),
            PublisherStoragePlan::<u64>::new(usize::MAX).with_subscriber(1),
        ] {
            assert!(matches!(
                plan.allocate(),
                Err(StorageError::CapacityOverflow)
            ));
        }
    }

    #[test]
    fn endpoint_construction_unwind_releases_loans_and_allows_rebuild() {
        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let mut plan = ChannelPlan::<Counted>::new("counted");
        let publisher_key = plan.publisher(1);
        let storage = plan.allocate().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let bindings = storage.build();
            let mut publisher = bindings.take_publisher(&publisher_key).unwrap();
            publisher.publish(Counted(drops.clone())).unwrap();
            panic!("another node failed during graph construction");
        }));
        assert!(result.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        let bindings = storage.build();
        let mut publisher = bindings.take_publisher(&publisher_key).unwrap();
        publisher.publish(Counted(drops.clone())).unwrap();
        drop(publisher);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }
}
