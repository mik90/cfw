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
