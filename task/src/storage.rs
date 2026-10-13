use std::marker::PhantomData;

use base::arena::Arena;

use super::{Publisher, Subscriber};
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
pub struct PublisherStoragePlan<T> {
    loan_capacity: usize,
    subscribers: Vec<usize>,
    retained_capacity: usize,
    payload: PhantomData<fn() -> T>,
}

impl<T> PublisherStoragePlan<T> {
    pub fn new(loan_capacity: usize) -> Self {
        Self {
            loan_capacity,
            subscribers: Vec::new(),
            retained_capacity: 0,
            payload: PhantomData,
        }
    }

    /// Add a subscriber, preserving its position in the built endpoint list.
    pub fn with_subscriber(mut self, capacity: usize) -> Self {
        self.subscribers.push(capacity);
        self
    }

    /// Set the additional budget for messages retained outside these queues.
    ///
    /// For forwarding, each downstream arena slot can retain a source message.
    /// Planning downstream capacities first provides a conservative source budget.
    /// Additional user-held pointers also consume this budget. Exceeding it yields
    /// `LoanError::ArenaExhausted`, never permission to reclaim a live message.
    pub fn with_retained_capacity(mut self, capacity: usize) -> Self {
        self.retained_capacity = capacity;
        self
    }

    /// Pending loans + retained messages + each subscriber's read/write queues
    /// and one pointer in flight while transferring between them.
    pub fn capacity(&self) -> Result<usize, StorageError> {
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
///
/// Retained messages borrow their storage, even after endpoints are dropped:
/// ```compile_fail,E0505
/// use task::{PublisherStoragePlan, time::FrameworkTime};
/// let storage = PublisherStoragePlan::<u64>::new(1).with_subscriber(1).allocate().unwrap();
/// let mut endpoints = storage.build();
/// endpoints.publisher.publish(42).unwrap();
/// endpoints.publisher.flush(FrameworkTime::from_nanoseconds(1));
/// endpoints.subscribers[0].update();
/// let message = endpoints.subscribers[0].input_forwardable().pop().unwrap();
/// drop(endpoints);
/// drop(storage);
/// assert_eq!(message.message, 42);
/// ```
pub struct PublisherStorage<MessageType> {
    arena: Arena<Message<MessageType>>,
    loan_capacity: usize,
    subscribers: Vec<usize>,
}

impl<MessageType> PublisherStorage<MessageType> {
    pub(crate) fn publisher(&self) -> Publisher<'_, MessageType> {
        Publisher::new(self.arena.allocator(), self.loan_capacity)
    }

    pub fn capacity(&self) -> usize {
        self.arena.capacity()
    }

    /// Create typed endpoints wired according to the finalized plan. The caller
    /// can move these endpoints into callbacks, graphs, or test fixture handles.
    pub fn build(&self) -> ChannelEndpoints<'_, MessageType> {
        let subscribers: Vec<_> = self
            .subscribers
            .iter()
            .map(|&capacity| Subscriber::new(capacity))
            .collect();
        let mut publisher = self.publisher();
        for subscriber in &subscribers {
            publisher.connect(subscriber);
        }
        ChannelEndpoints {
            publisher,
            subscribers,
        }
    }
}

pub struct ChannelEndpoints<'storage, T> {
    pub publisher: Publisher<'storage, T>,
    pub subscribers: Vec<Subscriber<'storage, T>>,
}

impl<T> PublisherStoragePlan<T> {
    pub fn validate(&self) -> Result<(), StorageError> {
        self.capacity().map(|_| ())
    }

    /// Validate the capacity and allocate this publisher's arena.
    pub fn allocate(self) -> Result<PublisherStorage<T>, StorageError> {
        Ok(PublisherStorage {
            arena: Arena::new(self.capacity()?),
            loan_capacity: self.loan_capacity,
            subscribers: self.subscribers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Forwarded, LoanError, time::FrameworkTime};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn timestamp() -> FrameworkTime {
        FrameworkTime::from_nanoseconds(1)
    }

    #[test]
    fn forwarding_retains_source_messages_across_endpoint_rebuilds() {
        let forwarding = PublisherStoragePlan::new(1).with_subscriber(1);
        let source = PublisherStoragePlan::new(1)
            .with_subscriber(1)
            .with_retained_capacity(forwarding.capacity().unwrap());
        let source = source.allocate().unwrap();
        let forwarding = forwarding.allocate().unwrap();
        let mut retained = Vec::new();
        for value in [42_u64, 100] {
            let mut source = source.build();
            let mut forwarding = forwarding.build();
            source.publisher.publish(value).unwrap();
            source.publisher.flush(timestamp());
            source.subscribers[0].update();
            let message = source.subscribers[0].input_forwardable().pop().unwrap();
            forwarding
                .publisher
                .publish(Forwarded::new(true, message))
                .unwrap();
            forwarding.publisher.flush(timestamp());
            forwarding.subscribers[0].update();
            retained.push(forwarding.subscribers[0].input_forwardable().pop().unwrap());
        }
        assert_eq!(retained[0].message.forwarded_messages.message, 42);
        assert_eq!(retained[1].message.forwarded_messages.message, 100);
    }

    #[test]
    fn nested_forwarding_retains_original_source_storage() {
        let last = PublisherStoragePlan::new(1).with_subscriber(1);
        let middle = PublisherStoragePlan::new(1)
            .with_subscriber(1)
            .with_retained_capacity(last.capacity().unwrap());
        let source = PublisherStoragePlan::new(1)
            .with_subscriber(1)
            .with_retained_capacity(middle.capacity().unwrap());
        let source_storage = source.allocate().unwrap();
        let middle_storage = middle.allocate().unwrap();
        let last_storage = last.allocate().unwrap();
        let mut source = source_storage.build();
        let mut middle = middle_storage.build();
        let mut last = last_storage.build();
        source.publisher.publish(42_u64).unwrap();
        source.publisher.flush(timestamp());
        source.subscribers[0].update();
        let input = source.subscribers[0].input_forwardable().pop().unwrap();
        middle
            .publisher
            .publish(Forwarded::new(true, input))
            .unwrap();
        middle.publisher.flush(timestamp());
        middle.subscribers[0].update();
        let input = middle.subscribers[0].input_forwardable().pop().unwrap();
        last.publisher
            .publish(Forwarded::new(7_u32, input))
            .unwrap();
        last.publisher.flush(timestamp());
        last.subscribers[0].update();
        let retained = last.subscribers[0].input_forwardable().pop().unwrap();
        drop((source, middle, last));
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
        let storage = plan.allocate().unwrap();
        assert_eq!(storage.capacity(), 7);
        let mut graph = storage.build();
        for value in [1, 2] {
            graph.publisher.publish(value).unwrap();
        }
        graph.publisher.flush(timestamp());
        graph.subscribers[0].update();
        for value in [3, 4] {
            graph.publisher.publish(value).unwrap();
        }
        graph.publisher.flush(timestamp());
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
        let storage = PublisherStoragePlan::<u64>::new(1)
            .with_subscriber(1)
            .with_retained_capacity(2)
            .allocate()
            .unwrap();
        let mut graph = storage.build();
        let mut retained = Vec::new();
        for value in 0..storage.capacity() {
            graph.publisher.publish(value as u64).unwrap();
            graph.publisher.flush(timestamp());
            graph.subscribers[0].update();
            retained.push(graph.subscribers[0].input_forwardable().pop().unwrap());
        }
        drop(graph);
        let mut graph = storage.build();
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
        let storage = PublisherStoragePlan::<Counted>::new(1).allocate().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut partially_built = storage.build();
            partially_built
                .publisher
                .publish(Counted(drops.clone()))
                .unwrap();
            panic!("another node failed during graph construction");
        }));
        assert!(result.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        let mut rebuilt = storage.build();
        rebuilt.publisher.publish(Counted(drops.clone())).unwrap();
        drop(rebuilt);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }
}
