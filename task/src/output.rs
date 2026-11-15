use base::arena::{ArenaAllocator, ArenaPtr, ArenaPtrUninit};

use crate::message::Message;
pub use crate::publisher::Publisher;
use alloc::vec::Vec;
use core::cell::Cell;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};

/// Exclusive in-place output reservation. A normal drop releases the slot
/// without dropping a potentially partially initialized payload. It is up
/// to the user to handle initialization failure.
pub struct Output<'output, 'graph_storage, T> {
    publisher: &'output mut Publisher<'graph_storage, T>,
    ptr: ArenaPtrUninit<'graph_storage, Message<T>>,
}

impl<'output, 'graph_storage, T> Output<'output, 'graph_storage, T> {
    pub fn new(
        publisher: &'output mut Publisher<'graph_storage, T>,
        ptr: ArenaPtrUninit<'graph_storage, Message<T>>,
    ) -> Self {
        Self { publisher, ptr }
    }

    /// The main use-case for [`Output`]: initialize large types in-place while
    /// handling safety and partially initialized values yourself.
    pub fn payload_uninit(&mut self) -> &mut MaybeUninit<T> {
        let message = self.ptr.payload_uninit().as_mut_ptr();
        // SAFETY: The loan exclusively owns Message<T>. MaybeUninit has T's
        // layout and does not require the payload field to be initialized.
        unsafe { &mut *(&raw mut (*message).message).cast::<MaybeUninit<T>>() }
    }

    pub fn write(mut self, value: T) -> OutputInit<'output, 'graph_storage, T> {
        self.payload_uninit().write(value);
        // SAFETY: write initialized T and loan_uninit initialized the header.
        unsafe { self.assume_init() }
    }

    /// # Safety
    /// The payload must be fully initialized.
    pub unsafe fn assume_init(self) -> OutputInit<'output, 'graph_storage, T> {
        OutputInit {
            publisher: self.publisher,
            // SAFETY: The caller initialized T; loan_uninit initialized the header.
            ptr: unsafe { self.ptr.assume_init() },
        }
    }
}

/// Exclusive initialized output. Sending transfers it into the pending batch;
/// dropping without sending destroys the payload and releases its slot.
pub struct OutputInit<'output, 'graph_storage, T> {
    publisher: &'output mut Publisher<'graph_storage, T>,
    ptr: ArenaPtr<'graph_storage, Message<T>>,
}

impl<T> OutputInit<'_, '_, T> {
    pub fn send(self) {
        self.publisher.pending.push(self.ptr);
    }

    pub fn value(&self) -> &T {
        // SAFETY: OutputInit contains a fully initialized, exclusively owned payload.
        &unsafe { (*self.ptr.payload.get()).assume_init_ref() }.message
    }

    pub fn value_mut(&mut self) -> &mut T {
        // SAFETY: No pointer clones are exposed until send followed by flush,
        // both of which end access through this exclusive output handle.
        &mut unsafe { (*self.ptr.payload.get()).assume_init_mut() }.message
    }
}

impl<T> Deref for OutputInit<'_, '_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value()
    }
}

impl<T> DerefMut for OutputInit<'_, '_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value_mut()
    }
}

/// Default-initialized single output endpoint. The executor constructs it with
/// `T::default()` before calling the callback.
pub type OutputDefault<'output, 'graph_storage, T> = OutputInit<'output, 'graph_storage, T>;

/// A callback-local span borrowing one publisher. Each reservation owns a
/// distinct arena slot. Outstanding and sent entries share the publisher's quota.
/// Drop unsent handles to return their slots and quota before reserving again.
pub struct OutputSpan<'output, 'graph_storage, T> {
    allocator: &'output ArenaAllocator<'graph_storage, Message<T>>,
    pending: &'output Cell<Vec<ArenaPtr<'graph_storage, Message<T>>>>,
    loan_capacity: usize,
    pending_len: Cell<usize>,
    outstanding: Cell<usize>,
}

/// Result of taking one entry from an [`OutputSpan`].
pub enum TakeEntry<T> {
    /// An entry was reserved.
    Taken(T),
    /// Every entry in the span is already reserved or sent.
    Full,
    /// The publisher arena is smaller than the span's declared capacity.
    ///
    /// This is an internal invariant violation: a planned publisher reserves
    /// at least its loan capacity, so this only happens for a manually
    /// constructed publisher with a mismatched arena.
    ArenaExhausted,
}

impl<'output, 'graph_storage, T> OutputSpan<'output, 'graph_storage, T> {
    pub(super) fn new(publisher: &'output mut Publisher<'graph_storage, T>) -> Self {
        Self {
            allocator: &publisher.allocator,
            loan_capacity: publisher.loan_capacity(),
            pending_len: Cell::new(publisher.pending.len()),
            pending: Cell::from_mut(&mut publisher.pending),
            outstanding: Cell::new(0),
        }
    }

    /// Takes an uninitialized entry, or returns [`None`] when the span is full.
    ///
    /// Panics if the publisher arena is smaller than the span's declared
    /// capacity; use [`Self::try_take_entry_uninit`] to observe that case.
    pub fn take_entry_uninit(
        &self,
    ) -> Option<OutputSpanEntryUninit<'_, 'output, 'graph_storage, T>> {
        match self.try_take_entry_uninit() {
            TakeEntry::Taken(entry) => Some(entry),
            TakeEntry::Full => None,
            TakeEntry::ArenaExhausted => {
                panic!("span entry allocation exceeded planned publisher arena capacity")
            }
        }
    }

    /// Takes an uninitialized entry, reporting the full-span and invariant
    /// cases explicitly.
    pub fn try_take_entry_uninit(
        &self,
    ) -> TakeEntry<OutputSpanEntryUninit<'_, 'output, 'graph_storage, T>> {
        if self.outstanding.get() >= (self.loan_capacity - self.pending_len.get()) {
            return TakeEntry::Full;
        }
        let Ok(ptr) = crate::publisher::allocate_output(self.allocator) else {
            return TakeEntry::ArenaExhausted;
        };
        self.outstanding.set(self.outstanding.get() + 1);
        TakeEntry::Taken(OutputSpanEntryUninit {
            ptr,
            reservation: Reservation { span: self },
        })
    }

    /// Takes an entry initialized with `value`, or returns [`None`] when the
    /// span is full.
    ///
    /// Panics if the publisher arena is smaller than the span's declared
    /// capacity; use [`Self::try_take_entry`] to observe that case.
    pub fn take_entry(&self, value: T) -> Option<OutputSpanEntry<'_, 'output, 'graph_storage, T>> {
        match self.try_take_entry(value) {
            TakeEntry::Taken(entry) => Some(entry),
            TakeEntry::Full => None,
            TakeEntry::ArenaExhausted => {
                panic!("span entry allocation exceeded planned publisher arena capacity")
            }
        }
    }

    /// Takes an entry initialized with `value`, reporting the full-span and
    /// invariant cases explicitly.
    pub fn try_take_entry(
        &self,
        value: T,
    ) -> TakeEntry<OutputSpanEntry<'_, 'output, 'graph_storage, T>> {
        match self.try_take_entry_uninit() {
            TakeEntry::Taken(entry) => TakeEntry::Taken(entry.write(value)),
            TakeEntry::Full => TakeEntry::Full,
            TakeEntry::ArenaExhausted => TakeEntry::ArenaExhausted,
        }
    }

    fn enqueue(&self, ptr: ArenaPtr<'graph_storage, Message<T>>) {
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

struct Reservation<'span, 'output, 'graph_storage, T> {
    span: &'span OutputSpan<'output, 'graph_storage, T>,
}

impl<T> Drop for Reservation<'_, '_, '_, T> {
    fn drop(&mut self) {
        self.span.outstanding.set(self.span.outstanding.get() - 1);
    }
}

/// Exclusive span reservation whose payload may be initialized in place.
/// Dropping it releases the slot without dropping a partially initialized T.
pub struct OutputSpanEntryUninit<'span, 'output, 'graph_storage, T> {
    ptr: ArenaPtrUninit<'graph_storage, Message<T>>,
    reservation: Reservation<'span, 'output, 'graph_storage, T>,
}

impl<'span, 'output, 'graph_storage, T> OutputSpanEntryUninit<'span, 'output, 'graph_storage, T> {
    pub fn payload_uninit(&mut self) -> &mut MaybeUninit<T> {
        let message = self.ptr.payload_uninit().as_mut_ptr();
        // SAFETY: This reservation exclusively owns its slot. MaybeUninit<T>
        // has T's layout and accepts an uninitialized payload field.
        unsafe { &mut *(&raw mut (*message).message).cast::<MaybeUninit<T>>() }
    }

    pub fn write(mut self, value: T) -> OutputSpanEntry<'span, 'output, 'graph_storage, T> {
        self.payload_uninit().write(value);
        // SAFETY: write initialized the payload; Publisher initialized the header.
        unsafe { self.assume_init() }
    }

    /// # Safety
    /// The entire payload must be initialized.
    pub unsafe fn assume_init(self) -> OutputSpanEntry<'span, 'output, 'graph_storage, T> {
        OutputSpanEntry {
            // SAFETY: The caller initialized T; Publisher initialized the header.
            ptr: unsafe { self.ptr.assume_init() },
            reservation: self.reservation,
        }
    }
}

/// Exclusive initialized reservation. Sending enqueues it in send order;
/// dropping it without sending destroys the payload and returns its quota.
pub struct OutputSpanEntry<'span, 'output, 'graph_storage, T> {
    ptr: ArenaPtr<'graph_storage, Message<T>>,
    reservation: Reservation<'span, 'output, 'graph_storage, T>,
}

impl<T> OutputSpanEntry<'_, '_, '_, T> {
    pub fn send(self) {
        self.reservation.span.enqueue(self.ptr);
    }

    pub fn value(&self) -> &T {
        // SAFETY: OutputInit contains a fully initialized, exclusively owned payload.
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
