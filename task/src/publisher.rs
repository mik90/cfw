use std::any::Any;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};

use crate::subscriber::SubscriberWriter;
use base::arena::{ArenaAllocator, ArenaPtr, ArenaPtrUninit};

use super::Subscriber;
use crate::message::{Message, MessageHeader};
use crate::time::FrameworkTime;

mod span;
pub use span::{OutputSpan, SpanOutput, SpanOutputUninit};

#[derive(Debug, PartialEq, Eq)]
pub enum LoanError {
    LoanCapacityReached,
    ArenaExhausted,
    Transport(String),
}

/// A typed publisher borrowing storage allocated before connection/execution.
/// Connection does not resize its arena: the graph plan owns capacity decisions.
pub struct Publisher<'storage, T> {
    allocator: ArenaAllocator<'storage, Message<T>>,
    loan_capacity: usize,
    pending: Vec<ArenaPtr<'storage, Message<T>>>,
    subscribers: Vec<SubscriberWriter<'storage, T>>,
    channel: String,
    observers: Vec<PublishObserver<'storage, T>>,
}
type PublishObserver<'a, T> = Box<dyn FnMut(&Message<T>) + Send + 'a>;

impl<'storage, T> Publisher<'storage, T> {
    pub fn new(allocator: ArenaAllocator<'storage, Message<T>>, loan_capacity: usize) -> Self {
        Self {
            allocator,
            loan_capacity,
            pending: Vec::with_capacity(loan_capacity),
            subscribers: Vec::new(),
            channel: String::new(),
            observers: Vec::new(),
        }
    }

    pub(crate) fn set_channel_name(&mut self, name: &str) {
        self.channel = name.into();
    }
    pub fn channel_name(&self) -> &str {
        &self.channel
    }

    pub fn connect(&mut self, subscriber: &Subscriber<'storage, T>) {
        self.subscribers.push(subscriber.writer());
    }
    /// Observe this publisher's initialized, stamped outputs before fan-out.
    /// References are scoped to the observer call and cannot escape it.
    pub fn observe(&mut self, observer: impl FnMut(&Message<T>) + Send + 'storage) {
        self.observers.push(Box::new(observer));
    }
    pub fn suppress_delivery(&mut self) {
        self.subscribers.clear();
    }

    pub fn loan_uninit(&mut self) -> Result<OutputUninit<'_, 'storage, T>, LoanError> {
        if self.pending.len() >= self.loan_capacity {
            return Err(LoanError::LoanCapacityReached);
        }
        let ptr = allocate_output(&self.allocator)?;
        Ok(OutputUninit {
            publisher: self,
            ptr,
        })
    }

    pub fn loan(&mut self, value: T) -> Result<Output<'_, 'storage, T>, LoanError> {
        self.loan_uninit().map(|loan| loan.write(value))
    }

    pub fn publish(&mut self, value: T) -> Result<(), LoanError> {
        self.loan(value)?.send();
        Ok(())
    }

    /// Borrow this publisher for a span of simultaneously outstanding loans.
    /// Sent outputs join the ordinary pending queue in send order; the executor
    /// stamps and publishes them only after the callback succeeds.
    ///
    /// ```
    /// use base::arena::Arena;
    /// use task::{Publisher, message::Message, time::FrameworkTime};
    /// let storage = Arena::<Message<u64>>::new(2);
    /// let mut publisher = Publisher::new(storage.allocator(), 2);
    /// {
    ///     let span = publisher.span();
    ///     let first = span.loan_uninit().unwrap();
    ///     let second = span.loan_uninit().unwrap();
    ///     let first = first.write(10);
    ///     let second = second.write(*first + 1);
    ///     second.send();
    ///     first.send();
    /// }
    /// publisher.flush(FrameworkTime::from_nanoseconds(123));
    /// ```
    pub fn span(&mut self) -> OutputSpan<'_, 'storage, T> {
        OutputSpan::new(self)
    }

    /// Release sent-but-unpublished outputs after a failed callback.
    pub fn discard_pending(&mut self) {
        self.pending.clear();
    }
    pub fn visit_pending_headers(&self, mut visit: impl FnMut(MessageHeader)) {
        for message in &self.pending {
            // SAFETY: pending contains initialized loans; publication/mutation
            // requires an exclusive publisher borrow.
            visit(unsafe { message.assume_init_ref() }.header);
        }
    }

    /// Publish a fully initialized batch at an executor-provided timestamp.
    /// Unsent outputs are released when their output handle is dropped.
    pub fn flush(&mut self, timestamp: FrameworkTime) {
        for ptr in self.pending.drain(..) {
            // SAFETY: Pending pointers are initialized and exclusively owned;
            // no clones are exposed before the header is stamped.
            unsafe { (*ptr.payload.get()).assume_init_mut().header.published_at = timestamp };
            for observer in &mut self.observers {
                // SAFETY: pending outputs are fully initialized and immutable for
                // observer access; stamping precedes observation and publication.
                observer(unsafe { ptr.assume_init_ref() });
            }
            for subscriber in &self.subscribers {
                subscriber.write(ptr.clone());
            }
        }
    }
}

fn allocate_output<'storage, T>(
    allocator: &ArenaAllocator<'storage, Message<T>>,
) -> Result<ArenaPtrUninit<'storage, Message<T>>, LoanError> {
    let mut ptr = allocator
        .try_allocate_uninit()
        .ok_or(LoanError::ArenaExhausted)?;
    let message = ptr.payload_uninit().as_mut_ptr();
    // SAFETY: This reservation exclusively owns storage. Raw field writes
    // initialize the header without creating a reference to uninitialized T.
    unsafe { (&raw mut (*message).header).write(MessageHeader::default()) };
    Ok(ptr)
}

/// Exclusive in-place output reservation. A normal drop releases the slot
/// without dropping a potentially partially initialized payload.
pub struct OutputUninit<'output, 'storage, T> {
    publisher: &'output mut Publisher<'storage, T>,
    ptr: ArenaPtrUninit<'storage, Message<T>>,
}

impl<'output, 'storage, T> OutputUninit<'output, 'storage, T> {
    pub fn payload_uninit(&mut self) -> &mut MaybeUninit<T> {
        let message = self.ptr.payload_uninit().as_mut_ptr();
        // SAFETY: The loan exclusively owns Message<T>. MaybeUninit has T's
        // layout and does not require the payload field to be initialized.
        unsafe { &mut *(&raw mut (*message).message).cast::<MaybeUninit<T>>() }
    }

    pub fn write(mut self, value: T) -> Output<'output, 'storage, T> {
        self.payload_uninit().write(value);
        // SAFETY: write initialized T and loan_uninit initialized the header.
        unsafe { self.assume_init() }
    }

    /// # Safety
    /// The payload must be fully initialized.
    pub unsafe fn assume_init(self) -> Output<'output, 'storage, T> {
        Output {
            publisher: self.publisher,
            // SAFETY: The caller initialized T; loan_uninit initialized the header.
            ptr: unsafe { self.ptr.assume_init() },
        }
    }
}

/// Exclusive initialized output. Sending transfers it into the pending batch;
/// dropping without sending destroys the payload and releases its slot.
pub struct Output<'output, 'storage, T> {
    publisher: &'output mut Publisher<'storage, T>,
    ptr: ArenaPtr<'storage, Message<T>>,
}

impl<T> Output<'_, '_, T> {
    pub fn send(self) {
        self.publisher.pending.push(self.ptr);
    }
}

impl<T> Deref for Output<'_, '_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: Output contains a fully initialized, exclusively owned payload.
        &unsafe { (*self.ptr.payload.get()).assume_init_ref() }.message
    }
}

impl<T> DerefMut for Output<'_, '_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: No pointer clones are exposed until send followed by flush,
        // both of which end access through this exclusive output handle.
        &mut unsafe { (*self.ptr.payload.get()).assume_init_mut() }.message
    }
}

/// Executor operations erase the message type without requiring Any on endpoints.
/// Store these as `Box<dyn PublisherOps + 'storage>` for heterogeneous graphs.
pub trait PublisherOps: Send {
    fn flush(&mut self, timestamp: FrameworkTime);
}

impl<T: Send + Sync> PublisherOps for Publisher<'_, T> {
    fn flush(&mut self, timestamp: FrameworkTime) {
        Publisher::flush(self, timestamp);
    }
}

/// Replay errors return ownership of the input so callers can retry capacity errors.
pub enum ReplayError {
    TypeMismatch(Box<dyn Any + Send>),
    Loan {
        reason: LoanError,
        value: Box<dyn Any + Send>,
    },
}

/// Replay erases owned payloads, not the borrowed publisher containing them.
/// Borrowed forwarded payloads use typed publishing instead of this interface.
pub trait ReplayPublisher: PublisherOps {
    fn publish_boxed(&mut self, value: Box<dyn Any + Send>) -> Result<(), ReplayError>;
}

impl<T: Send + Sync + 'static> ReplayPublisher for Publisher<'_, T> {
    fn publish_boxed(&mut self, value: Box<dyn Any + Send>) -> Result<(), ReplayError> {
        let value = value.downcast::<T>().map_err(ReplayError::TypeMismatch)?;
        match self.loan_uninit() {
            Ok(loan) => {
                loan.write(*value).send();
                Ok(())
            }
            Err(reason) => Err(ReplayError::Loan { reason, value }),
        }
    }
}
