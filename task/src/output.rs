use base::arena::{ArenaAllocator, ArenaPtr, ArenaPtrUninit};

use crate::message::Message;
use crate::publisher::LoanError;
pub use crate::publisher::Publisher;
use std::cell::Cell;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};

/// Exclusive in-place output reservation. A normal drop releases the slot
/// without dropping a potentially partially initialized payload. It is up
/// to the user to handle initialization failure.
pub struct OutputUninit<'output, 'storage, T> {
    publisher: &'output mut Publisher<'storage, T>,
    ptr: ArenaPtrUninit<'storage, Message<T>>,
}

impl<'output, 'storage, T> OutputUninit<'output, 'storage, T> {
    pub fn new(
        publisher: &'output mut Publisher<'storage, T>,
        ptr: ArenaPtrUninit<'storage, Message<T>>,
    ) -> Self {
        Self { publisher, ptr }
    }

    /// The main use-case for OutputUninit, this allows users to initialize large
    /// types in-place but they must handle safety on their own, including managing
    /// partially initialized values.
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

    pub fn value(&self) -> &T {
        // SAFETY: Output contains a fully initialized, exclusively owned payload.
        &unsafe { (*self.ptr.payload.get()).assume_init_ref() }.message
    }

    pub fn value_mut(&mut self) -> &mut T {
        // SAFETY: No pointer clones are exposed until send followed by flush,
        // both of which end access through this exclusive output handle.
        &mut unsafe { (*self.ptr.payload.get()).assume_init_mut() }.message
    }
}

impl<T> Deref for Output<'_, '_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value()
    }
}

impl<T> DerefMut for Output<'_, '_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value_mut()
    }
}

/// A callback-local span borrowing one publisher. Each reservation owns a
/// distinct arena slot. Outstanding and sent loans share the publisher's quota.
/// Drop unsent handles to return their slots and quota before reserving again.
pub struct OutputSpan<'output, 'storage, T> {
    allocator: &'output ArenaAllocator<'storage, Message<T>>,
    pending: &'output Cell<Vec<ArenaPtr<'storage, Message<T>>>>,
    loan_capacity: usize,
    pending_len: Cell<usize>,
    outstanding: Cell<usize>,
}

impl<'output, 'storage, T> OutputSpan<'output, 'storage, T> {
    pub(super) fn new(publisher: &'output mut Publisher<'storage, T>) -> Self {
        Self {
            allocator: &publisher.allocator,
            loan_capacity: publisher.loan_capacity(),
            pending_len: Cell::new(publisher.pending.len()),
            pending: Cell::from_mut(&mut publisher.pending),
            outstanding: Cell::new(0),
        }
    }

    pub fn loan_uninit(&self) -> Result<SpanOutputUninit<'_, 'output, 'storage, T>, LoanError> {
        if self.outstanding.get() >= (self.loan_capacity - self.pending_len.get()) {
            return Err(LoanError::LoanCapacityReached);
        }
        let ptr = crate::publisher::allocate_output(self.allocator)?;
        self.outstanding.set(self.outstanding.get() + 1);
        Ok(SpanOutputUninit {
            ptr,
            reservation: Reservation { span: self },
        })
    }

    pub fn loan(&self, value: T) -> Result<OutputSpanEntry<'_, 'output, 'storage, T>, LoanError> {
        self.loan_uninit().map(|loan| loan.write(value))
    }

    fn enqueue(&self, ptr: ArenaPtr<'storage, Message<T>>) {
        // The publisher exclusively lends its queue for the span's lifetime.
        // Reservations enforce (pending + outstanding) <= loan_capacity, and the
        // queue is preallocated to that capacity. Pushing this reserved slot
        // cannot grow the allocation or invoke user code while the Cell is empty.
        let mut pending = self.pending.take();
        pending.push(ptr);
        self.pending_len.set(pending.len());
        self.pending.set(pending);
    }
}

struct Reservation<'span, 'output, 'storage, T> {
    span: &'span OutputSpan<'output, 'storage, T>,
}

impl<T> Drop for Reservation<'_, '_, '_, T> {
    fn drop(&mut self) {
        self.span.outstanding.set(self.span.outstanding.get() - 1);
    }
}

/// Exclusive span reservation whose payload may be initialized in place.
/// Dropping it releases the slot without dropping a partially initialized T.
pub struct SpanOutputUninit<'span, 'output, 'storage, T> {
    ptr: ArenaPtrUninit<'storage, Message<T>>,
    reservation: Reservation<'span, 'output, 'storage, T>,
}

impl<'span, 'output, 'storage, T> SpanOutputUninit<'span, 'output, 'storage, T> {
    pub fn payload_uninit(&mut self) -> &mut MaybeUninit<T> {
        let message = self.ptr.payload_uninit().as_mut_ptr();
        // SAFETY: This reservation exclusively owns its slot. MaybeUninit<T>
        // has T's layout and accepts an uninitialized payload field.
        unsafe { &mut *(&raw mut (*message).message).cast::<MaybeUninit<T>>() }
    }

    pub fn write(mut self, value: T) -> OutputSpanEntry<'span, 'output, 'storage, T> {
        self.payload_uninit().write(value);
        // SAFETY: write initialized the payload; Publisher initialized the header.
        unsafe { self.assume_init() }
    }

    /// # Safety
    /// The entire payload must be initialized.
    pub unsafe fn assume_init(self) -> OutputSpanEntry<'span, 'output, 'storage, T> {
        OutputSpanEntry {
            // SAFETY: The caller initialized T; Publisher initialized the header.
            ptr: unsafe { self.ptr.assume_init() },
            reservation: self.reservation,
        }
    }
}

/// Exclusive initialized reservation. Sending enqueues it in send order;
/// dropping it without sending destroys the payload and returns its quota.
pub struct OutputSpanEntry<'span, 'output, 'storage, T> {
    ptr: ArenaPtr<'storage, Message<T>>,
    reservation: Reservation<'span, 'output, 'storage, T>,
}

impl<T> OutputSpanEntry<'_, '_, '_, T> {
    pub fn send(self) {
        self.reservation.span.enqueue(self.ptr);
    }

    pub fn value(&self) -> &T {
        // SAFETY: Output contains a fully initialized, exclusively owned payload.
        &unsafe { (*self.ptr.payload.get()).assume_init_ref() }.message
    }

    pub fn value_mut(&mut self) -> &mut T {
        // SAFETY: No pointer clones are exposed until send followed by flush,
        // both of which end access through this exclusive output handle.
        &mut unsafe { (*self.ptr.payload.get()).assume_init_mut() }.message
    }
}

impl<T> Deref for OutputSpanEntry<'_, '_, '_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value()
    }
}
impl<T> DerefMut for OutputSpanEntry<'_, '_, '_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value_mut()
    }
}
