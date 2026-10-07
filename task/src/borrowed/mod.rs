//! Experimental typed endpoints borrowing externally owned arena storage.
//!
//! Built independently as `borrowed-task` while the graph/executor APIs migrate.
//! Message headers and time types use the existing task implementations.
//! Endpoint type erasure preserves storage lifetimes and exposes operations;
//! only owned replay payloads are downcast through `Any`.

mod publisher;
mod storage;
mod subscriber;

pub use publisher::{
    LoanError, Output, OutputUninit, Publisher, PublisherOps, ReplayError, ReplayPublisher,
};
pub use storage::{
    ChannelEndpoints, ChannelPlan, ChannelStorage, GraphPlan, GraphStorage, StorageError,
    StorageLayout,
};
pub use subscriber::{Input, Subscriber};

use crate::message::Message;
use base::arena::ArenaReaderPtr;

/// Forwarding retains the source message for the common storage borrow.
/// Source and destination arenas may be distinct allocations.
pub struct ForwardedMessage<'storage, T, F> {
    pub message: T,
    pub forwarded: ArenaReaderPtr<'storage, Message<F>>,
}

impl<'storage, T, F> ForwardedMessage<'storage, T, F> {
    pub fn new(message: T, forwarded: ArenaReaderPtr<'storage, Message<F>>) -> Self {
        Self { message, forwarded }
    }
}
