use super::{LoanError, Publisher};
use crate::message::Message;
use base::arena::{ArenaPtr, ArenaPtrUninit};
use std::{
    cell::{Cell, RefCell},
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
};

/// A callback-local batch borrowing one publisher. Each reservation owns a
/// distinct arena slot. Outstanding and sent loans share the publisher's quota.
/// Drop unsent handles to return their slots and quota before reserving again.
pub struct OutputBatch<'output, 'storage, T> {
    publisher: RefCell<&'output mut Publisher<'storage, T>>,
    outstanding: Cell<usize>,
}

impl<'output, 'storage, T> OutputBatch<'output, 'storage, T> {
    pub(super) fn new(publisher: &'output mut Publisher<'storage, T>) -> Self {
        Self {
            publisher: RefCell::new(publisher),
            outstanding: Cell::new(0),
        }
    }

    pub fn loan_uninit(&self) -> Result<BatchOutputUninit<'_, 'output, 'storage, T>, LoanError> {
        let mut publisher = self.publisher.borrow_mut();
        if self.outstanding.get() >= publisher.loan_capacity - publisher.pending.len() {
            return Err(LoanError::LoanCapacityReached);
        }
        let super::OutputUninit { ptr, .. } = publisher.loan_uninit()?;
        self.outstanding.set(self.outstanding.get() + 1);
        Ok(BatchOutputUninit {
            ptr,
            reservation: Reservation { batch: self },
        })
    }

    pub fn loan(&self, value: T) -> Result<BatchOutput<'_, 'output, 'storage, T>, LoanError> {
        self.loan_uninit().map(|loan| loan.write(value))
    }
}

struct Reservation<'batch, 'output, 'storage, T> {
    batch: &'batch OutputBatch<'output, 'storage, T>,
}
impl<T> Drop for Reservation<'_, '_, '_, T> {
    fn drop(&mut self) {
        self.batch.outstanding.set(self.batch.outstanding.get() - 1);
    }
}

/// Exclusive batch reservation whose payload may be initialized in place.
/// Dropping it releases the slot without dropping a partially initialized T.
pub struct BatchOutputUninit<'batch, 'output, 'storage, T> {
    ptr: ArenaPtrUninit<'storage, Message<T>>,
    reservation: Reservation<'batch, 'output, 'storage, T>,
}
impl<'batch, 'output, 'storage, T> BatchOutputUninit<'batch, 'output, 'storage, T> {
    pub fn payload_uninit(&mut self) -> &mut MaybeUninit<T> {
        let message = self.ptr.payload_uninit().as_mut_ptr();
        // SAFETY: This reservation exclusively owns its slot. MaybeUninit<T>
        // has T's layout and accepts an uninitialized payload field.
        unsafe { &mut *(&raw mut (*message).message).cast::<MaybeUninit<T>>() }
    }

    pub fn write(mut self, value: T) -> BatchOutput<'batch, 'output, 'storage, T> {
        self.payload_uninit().write(value);
        // SAFETY: write initialized the payload; Publisher initialized the header.
        unsafe { self.assume_init() }
    }

    /// # Safety
    /// The entire payload must be initialized.
    pub unsafe fn assume_init(self) -> BatchOutput<'batch, 'output, 'storage, T> {
        BatchOutput {
            // SAFETY: The caller initialized T; Publisher initialized the header.
            ptr: unsafe { self.ptr.assume_init() },
            reservation: self.reservation,
        }
    }
}

/// Exclusive initialized reservation. Sending enqueues it in send order;
/// dropping it without sending destroys the payload and returns its quota.
pub struct BatchOutput<'batch, 'output, 'storage, T> {
    ptr: ArenaPtr<'storage, Message<T>>,
    reservation: Reservation<'batch, 'output, 'storage, T>,
}
impl<T> BatchOutput<'_, '_, '_, T> {
    pub fn send(self) {
        self.reservation
            .batch
            .publisher
            .borrow_mut()
            .pending
            .push(self.ptr);
    }
}
impl<T> Deref for BatchOutput<'_, '_, '_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: Initialized slot, exclusively owned until send and flush.
        &unsafe { self.ptr.assume_init_ref() }.message
    }
}
impl<T> DerefMut for BatchOutput<'_, '_, '_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: Each reservation has a distinct slot, with no clones exposed.
        &mut unsafe { (*self.ptr.payload.get()).assume_init_mut() }.message
    }
}
