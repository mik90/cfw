use std::marker::PhantomData;

use base::arena::Arena;

use super::{Publisher, Subscriber};
use crate::message::Message;

/// Capacity errors are detected before allocating any arena in a graph layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
    ZeroSubscriberCapacity,
    CapacityOverflow,
}

/// Storage requirements for one publisher and its subscribers.
///
/// `T` can contain borrowed forwarded messages; no `Any` or `'static` bound is
/// required. Planning neither allocates arenas nor borrows their storage.
pub struct ChannelPlan<T> {
    loan_capacity: usize,
    subscribers: Vec<usize>,
    retained_capacity: usize,
    payload: PhantomData<fn() -> T>,
}

impl<T> ChannelPlan<T> {
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

/// A typed graph layout before storage allocation. Pairs compose recursively, so
/// layouts can contain arbitrary numbers of differently typed channels.
pub struct GraphPlan<L> {
    layout: L,
}

impl<L: StorageLayout> GraphPlan<L> {
    pub fn new(layout: L) -> Self {
        Self { layout }
    }

    /// Validate the entire layout, then consume it to allocate fixed storage.
    pub fn allocate(self) -> Result<GraphStorage<L::Storage>, StorageError> {
        self.layout.validate()?;
        Ok(GraphStorage {
            channels: self.layout.allocate(),
        })
    }
}

/// External owner of typed, heterogeneous channel arenas.
///
/// Endpoints and retained messages borrow this owner. Storage has no payload-
/// traversing destructor: initialized payloads are destroyed by their last arena
/// pointer, not by graph storage. This permits forwarding between arenas within
/// one owner without storing Rust references in the owner's structural fields.
///
/// ```
/// use borrowed_task::{ChannelPlan, GraphPlan};
/// let storage = GraphPlan::new((
///     ChannelPlan::<u64>::new(1).with_subscriber(2),
///     ChannelPlan::<String>::new(1).with_subscriber(1),
/// )).allocate().unwrap();
/// let numbers = storage.channels().0.build();
/// let strings = storage.channels().1.build();
/// ```
///
/// A retained message prevents destruction of the storage owner:
/// ```compile_fail
/// use borrowed_task::{ChannelPlan, GraphPlan};
/// use borrowed_task::time::FrameworkTime;
/// let storage = GraphPlan::new(ChannelPlan::<u64>::new(1).with_subscriber(1))
///     .allocate().unwrap();
/// let mut endpoints = storage.channels().build();
/// endpoints.publisher.publish(42).unwrap();
/// endpoints.publisher.flush(FrameworkTime::from_nanoseconds(1));
/// endpoints.subscribers[0].update();
/// let message = endpoints.subscribers[0].input().pop().unwrap();
/// drop(endpoints);
/// drop(storage);
/// assert_eq!(message.message, 42);
/// ```
pub struct GraphStorage<S> {
    channels: S,
}

impl<S> GraphStorage<S> {
    pub fn channels(&self) -> &S {
        &self.channels
    }
}

/// Frozen per-channel allocation. All builds share this channel's fixed budget.
/// A fresh build does not reset or reclaim messages retained from a prior build.
pub struct ChannelStorage<T> {
    arena: Arena<Message<T>>,
    loan_capacity: usize,
    subscribers: Vec<usize>,
}

impl<T> ChannelStorage<T> {
    pub fn capacity(&self) -> usize {
        self.arena.capacity()
    }

    /// Create typed endpoints wired according to the finalized plan. The caller
    /// can move these endpoints into callbacks, graphs, or test fixture handles.
    pub fn build(&self) -> ChannelEndpoints<'_, T> {
        let subscribers: Vec<_> = self
            .subscribers
            .iter()
            .map(|&capacity| Subscriber::new(capacity))
            .collect();
        let mut publisher = Publisher::new(self.arena.allocator(), self.loan_capacity);
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

mod private {
    pub trait Sealed {}
}

/// A channel plan, an empty layout, or a pair of layouts.
/// Sealed so validation and allocation cannot disagree in external implementations.
pub trait StorageLayout: private::Sealed {
    type Storage;

    #[doc(hidden)]
    fn validate(&self) -> Result<(), StorageError>;
    #[doc(hidden)]
    fn allocate(self) -> Self::Storage;
}

impl<T> private::Sealed for ChannelPlan<T> {}

impl<T> StorageLayout for ChannelPlan<T> {
    type Storage = ChannelStorage<T>;

    fn validate(&self) -> Result<(), StorageError> {
        self.capacity().map(|_| ())
    }

    fn allocate(self) -> Self::Storage {
        ChannelStorage {
            arena: Arena::new(self.capacity().expect("storage plan must be validated")),
            loan_capacity: self.loan_capacity,
            subscribers: self.subscribers,
        }
    }
}

impl private::Sealed for () {}

impl StorageLayout for () {
    type Storage = ();

    fn validate(&self) -> Result<(), StorageError> {
        Ok(())
    }
    fn allocate(self) -> Self::Storage {}
}

impl<L: StorageLayout, R: StorageLayout> private::Sealed for (L, R) {}

impl<L: StorageLayout, R: StorageLayout> StorageLayout for (L, R) {
    type Storage = (L::Storage, R::Storage);

    fn validate(&self) -> Result<(), StorageError> {
        self.0.validate()?;
        self.1.validate()
    }

    fn allocate(self) -> Self::Storage {
        (self.0.allocate(), self.1.allocate())
    }
}
