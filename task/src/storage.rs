use crate::string_interner::ChannelNameInterner;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::Arc;

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
    DuplicateChannel(String),
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

/// A typed graph layout before storage allocation. Pairs compose recursively;
/// multiple named channels may use the same payload type.
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
        let mut names = HashSet::new();
        self.layout.validate_channel_names(&mut names)?;
        let mut names: Vec<_> = names.into_iter().collect();
        names.sort();
        let mut channel_names = ChannelNameInterner::new();
        for name in names {
            channel_names.intern(&name);
        }
        channel_names.shrink_to_fit();
        Ok(GraphStorage {
            channels: self.layout.allocate()?,
            channel_names: Arc::new(channel_names),
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
/// use task::{ChannelPlan, GraphPlan};
/// let mut counter = ChannelPlan::<u64>::new("counter");
/// let counter_pub = counter.publisher(1);
/// let counter_sub = counter.subscriber(2);
/// let mut processed = ChannelPlan::<u64>::new("processed");
/// let processed_pub = processed.publisher(1);
/// let processed_sub = processed.subscriber(1);
/// let storage = GraphPlan::new((counter, processed)).allocate().unwrap();
/// let counter = storage.channels().0.build();
/// let processed = storage.channels().1.build();
/// let publisher = counter.take_publisher(&counter_pub).unwrap();
/// let subscriber = processed.take_subscriber(&processed_sub).unwrap();
/// ```
///
/// A retained message prevents destruction of the storage owner:
/// ```compile_fail
/// use task::{PublisherStoragePlan, GraphPlan};
/// use task::time::FrameworkTime;
/// let storage = GraphPlan::new(PublisherStoragePlan::<u64>::new(1).with_subscriber(1))
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
    channel_names: Arc<ChannelNameInterner>,
}

impl<S> GraphStorage<S> {
    pub fn channel_names(&self) -> &Arc<ChannelNameInterner> {
        &self.channel_names
    }
    pub fn channels(&self) -> &S {
        &self.channels
    }
}

/// Frozen per-publisher allocation. All builds share this publisher's fixed budget.
/// A fresh build does not reset or reclaim messages retained from a prior build.
pub struct PublisherStorage<T> {
    arena: Arena<Message<T>>,
    loan_capacity: usize,
    subscribers: Vec<usize>,
}

impl<T> PublisherStorage<T> {
    pub(crate) fn publisher(&self) -> Publisher<'_, T> {
        Publisher::new(self.arena.allocator(), self.loan_capacity)
    }

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

pub(super) mod private {
    pub trait Sealed {}
}

/// A publisher storage plan, a named channel plan, an empty layout, or a pair of layouts.
/// Sealed so validation and allocation cannot disagree in external implementations.
pub trait StorageLayout: private::Sealed {
    type Storage;

    #[doc(hidden)]
    fn validate(&self) -> Result<(), StorageError>;
    #[doc(hidden)]
    fn validate_channel_names(&self, _names: &mut HashSet<String>) -> Result<(), StorageError> {
        Ok(())
    }
    #[doc(hidden)]
    fn allocate(self) -> Result<Self::Storage, StorageError>;
}

impl<T> private::Sealed for PublisherStoragePlan<T> {}

impl<T> StorageLayout for PublisherStoragePlan<T> {
    type Storage = PublisherStorage<T>;

    fn validate(&self) -> Result<(), StorageError> {
        self.capacity().map(|_| ())
    }

    fn allocate(self) -> Result<Self::Storage, StorageError> {
        Ok(PublisherStorage {
            arena: Arena::new(self.capacity().expect("storage plan must be validated")),
            loan_capacity: self.loan_capacity,
            subscribers: self.subscribers,
        })
    }
}

impl private::Sealed for () {}

impl StorageLayout for () {
    type Storage = ();

    fn validate(&self) -> Result<(), StorageError> {
        Ok(())
    }
    fn allocate(self) -> Result<Self::Storage, StorageError> {
        Ok(())
    }
}

impl<L: StorageLayout, R: StorageLayout> private::Sealed for (L, R) {}

impl<L: StorageLayout, R: StorageLayout> StorageLayout for (L, R) {
    type Storage = (L::Storage, R::Storage);

    fn validate(&self) -> Result<(), StorageError> {
        self.0.validate()?;
        self.1.validate()
    }

    fn allocate(self) -> Result<Self::Storage, StorageError> {
        Ok((self.0.allocate()?, self.1.allocate()?))
    }

    fn validate_channel_names(&self, names: &mut HashSet<String>) -> Result<(), StorageError> {
        self.0.validate_channel_names(names)?;
        self.1.validate_channel_names(names)
    }
}
