use crate::forwarded_message::ForwardedMessage;
use crate::generic_publisher::GenericPublisher;
use crate::message::Message;
use crate::publisher::{ForwardingPublisher, LoanedValue, Publisher};
use base::arena::{Arena, ArenaPtrUninit, ArenaReaderPtr};
use std::cell::Cell;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};

pub struct ForwardingOutput<'a, T, F> {
    pub(crate) publisher: &'a mut ForwardingPublisher<T, F>,
}

impl<'a, T, F> ForwardingOutput<'a, T, F> {
    pub fn new(publisher: &'a mut ForwardingPublisher<T, F>) -> Self {
        Self { publisher }
    }
}

// This downcast uses `Any`, so both payload component types must be `'static`.
impl<'a, T: 'static, F: 'static> ForwardingOutput<'a, T, F> {
    pub fn new_downcasted(publisher: &'a mut dyn GenericPublisher) -> Self {
        ForwardingOutput::new(ForwardingPublisher::new_downcasted(publisher))
    }
}

pub struct Output<'a, T> {
    pub(crate) loaned_value: &'a mut LoanedValue<T>,
}

impl<'a, T> Output<'a, T> {
    pub fn value(&self) -> &T {
        self.loaned_value.payload()
    }

    pub fn value_mut(&mut self) -> &mut T {
        self.loaned_value.payload_mut()
    }

    pub fn send(self) {
        self.loaned_value.sent = true;
    }

    pub(crate) fn new_with_factory(
        publisher: &'a mut Publisher<T>,
        factory: impl FnOnce(&mut MaybeUninit<T>),
    ) -> Self {
        let loaned_value_idx = publisher
            .loan_with(factory)
            .expect("We expect loans to always be available");
        Output {
            loaned_value: publisher.loaned_value_at_mut(loaned_value_idx),
        }
    }
}

impl<'a, T: Default> Output<'a, T> {
    pub fn new_default(publisher: &'a mut Publisher<T>) -> Self {
        let loaned_value_idx = publisher
            .loan_default()
            .expect("We expect loans to always be available");
        Output {
            loaned_value: publisher.loaned_value_at_mut(loaned_value_idx),
        }
    }
}

// `new_downcasted` uses `Any`; only `'static` is needed for that type check.
impl<'a, T: Default + 'static> Output<'a, T> {
    pub fn new_downcasted_default(publisher: &mut dyn GenericPublisher) -> Output<'_, T> {
        let typed_publisher = publisher.as_any().downcast_mut::<Publisher<T>>();
        Output::new_default(typed_publisher.expect("Expected proc macro to use the correct types"))
    }
}

impl<T> Deref for Output<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value()
    }
}

impl<T> DerefMut for Output<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value_mut()
    }
}

struct UninitLoanSink<'publisher, T> {
    // We'll swap this out once we're done
    destination: &'publisher mut Vec<LoanedValue<T>>,
    // Staged loans that'll be given to the publisher on drop
    pending: Cell<Vec<LoanedValue<T>>>,
}

impl<'publisher, T> UninitLoanSink<'publisher, T> {
    fn new(destination: &'publisher mut Vec<LoanedValue<T>>, loan_count: usize) -> Self {
        if loan_count > (destination.capacity() - destination.len()) {
            panic!(
                "Not enough loans are left: {} > ({} - {})",
                loan_count,
                destination.capacity(),
                destination.len()
            );
        }

        let pending = Cell::new(std::mem::take(destination));

        Self {
            destination,
            pending,
        }
    }

    fn push(&self, loan: LoanedValue<T>) {
        let mut pending = self.pending.take();
        // We check that loans will be less than capacity in new() to avoid allocations here
        pending.push(loan);
        self.pending.set(pending);
    }
}

impl<T> Drop for UninitLoanSink<'_, T> {
    /// Swap the pending loans back into the publisher's vec
    fn drop(&mut self) {
        *self.destination = std::mem::take(self.pending.get_mut());
    }
}

enum LoanRouter<'output, 'publisher, T> {
    /// Directly modified loan value vec on publisher
    Direct(&'output mut Vec<LoanedValue<T>>),
    /// Staging area for uninit spans
    Staged(&'output UninitLoanSink<'publisher, T>),
}

pub struct OutputUninit<'output, 'publisher, T> {
    destination: LoanRouter<'output, 'publisher, T>,
    ptr: ArenaPtrUninit<Message<T>>,
}

impl<'output, 'publisher, T> OutputUninit<'output, 'publisher, T> {
    pub fn new(publisher: &'publisher mut Publisher<T>) -> Self {
        let arena_ptr_uninit = publisher
            .loan_uninit()
            .expect("We expect loans to always be available");
        let loaned_values = publisher.loaned_values_mut();
        OutputUninit {
            destination: LoanRouter::Direct(loaned_values),
            ptr: arena_ptr_uninit,
        }
    }

    /// Provides exclusive access to the uninitialized payload storage.
    /// The framework initializes and manages the message header.
    pub fn value_uninit(&mut self) -> &mut MaybeUninit<T> {
        let msg_ptr = self.ptr.payload_uninit().as_mut_ptr();
        // SAFETY: The loan provides exclusive payload storage. MaybeUninit<T>
        // has the same size and alignment as T; no initialized T reference is formed.
        unsafe { &mut *(&raw mut (*msg_ptr).message).cast::<MaybeUninit<T>>() }
    }

    /// # Safety
    ///
    /// The payload T must be fully initialized before calling this.
    /// The framework guarantees that the header is already initialized.
    pub unsafe fn send_assume_init(self) {
        let loaned_value = LoanedValue {
            // SAFETY: The caller guarantees an initialized payload, and
            // Publisher::loan_uninit initialized the header.
            ptr: unsafe { self.ptr.assume_init() },
            sent: true,
        };
        match self.destination {
            LoanRouter::Direct(v) => v.push(loaned_value),
            LoanRouter::Staged(s) => s.push(loaned_value),
        }
    }
}

pub struct OutputSpan<'a, T> {
    loaned_value_idx_start: usize,
    loaned_value_idx_end: usize,
    publisher: &'a mut Publisher<T>,
}

impl<'a, T> OutputSpan<'a, T> {
    pub fn outputs(&self) -> impl Iterator<Item = &T> {
        self.publisher
            .loaned_values_at(self.loaned_value_idx_start, self.loaned_value_idx_end)
            .iter()
            .map(|loaned_value|
                // SAFETY: Publisher guarantees the value has been initialized on loan
                // and a loaned value is exclusive access.
                unsafe { &(*loaned_value.ptr.payload.get()).assume_init_ref().message })
    }

    pub fn outputs_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.publisher
            .loaned_values_at_mut(self.loaned_value_idx_start, self.loaned_value_idx_end)
            .iter_mut()
            .map(|loaned_value|
                // SAFETY: Publisher guarantees the value has been initialized on loan
                // and a loaned value is exclusive access.
                unsafe { &mut (*loaned_value.ptr.payload.get()).assume_init_mut().message })
    }

    pub(crate) fn new_with_factory(
        publisher: &'a mut Publisher<T>,
        mut factory: impl FnMut(&mut MaybeUninit<T>),
    ) -> Self {
        let count = publisher.config().capacity;
        let start = publisher.loaned_count();
        for _ in 0..count {
            publisher.loan_with(|slot| factory(slot)).unwrap();
        }
        OutputSpan {
            loaned_value_idx_start: start,
            loaned_value_idx_end: start + count - 1,
            publisher,
        }
    }
}

impl<'a, T: Default> OutputSpan<'a, T> {
    pub fn new_default(publisher: &'a mut Publisher<T>) -> Self {
        for _ in 0..publisher.config().capacity {
            publisher.loan_default().unwrap();
        }
        OutputSpan {
            loaned_value_idx_start: 0,
            loaned_value_idx_end: publisher.config().capacity - 1,
            publisher,
        }
    }
}

pub struct OutputUninitSpan<'publisher, T> {
    arena: &'publisher mut Arena<Message<T>>,
    staging: UninitLoanSink<'publisher, T>,
    remaining_loans: usize,
}

impl<'publisher, T> OutputUninitSpan<'publisher, T> {
    pub fn new(publisher: &'publisher mut Publisher<T>) -> Self {
        let remaining_loans = publisher.config().capacity;
        let (loan_vec, arena) = publisher.uninit_loan_parts();
        OutputUninitSpan {
            arena,
            staging: UninitLoanSink::new(loan_vec, remaining_loans),
            remaining_loans,
        }
    }
}

pub struct OutputUninitSpanIterator<'iter, 'publisher, T> {
    arena: &'iter mut Arena<Message<T>>,
    staging: &'iter UninitLoanSink<'publisher, T>,
    remaining_loans: &'iter mut usize,
}
impl<'iter, 'publisher, T> OutputUninitSpanIterator<'iter, 'publisher, T> {}

impl<'iter, 'publisher, T> Iterator for OutputUninitSpanIterator<'iter, 'publisher, T> {
    type Item = OutputUninit<'iter, 'publisher, T>;

    fn next(&mut self) -> Option<Self::Item> {
        let arena_ptr_uninit = self.arena.try_allocate_uninit()?;
        *self.remaining_loans -= 1;

        Some(OutputUninit {
            destination: LoanRouter::Staged(self.staging),
            ptr: arena_ptr_uninit,
        })
    }
}

pub struct ForwardedOutput<'a, T, F> {
    inner: Output<'a, ForwardedMessage<T, F>>,
}

impl<'a, T, F> ForwardedOutput<'a, T, F> {
    pub fn value(&self) -> &T {
        &self.inner.value().message
    }

    pub fn value_mut(&mut self) -> &mut T {
        &mut self.inner.value_mut().message
    }
}

impl<'a, T: Default, F> ForwardedOutput<'a, T, F> {
    pub(crate) fn new(
        publisher: &'a mut ForwardingPublisher<T, F>,
        forwarded_ptr: ArenaReaderPtr<Message<F>>,
    ) -> Self {
        let loaned_value_idx = publisher
            .inner
            .loan_forwarded(forwarded_ptr)
            .expect("We expect loans to always be available");

        let output = Output {
            loaned_value: publisher.inner.loaned_value_at_mut(loaned_value_idx),
        };
        ForwardedOutput { inner: output }
    }

    pub fn send(self) {
        self.inner.send();
    }
}

impl<T, F> Deref for ForwardedOutput<'_, T, F> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value()
    }
}

impl<T, F> DerefMut for ForwardedOutput<'_, T, F> {
    fn deref_mut(&mut self) -> &mut T {
        self.value_mut()
    }
}

pub struct ForwardedOutputSpan<'a, T, F> {
    inner: OutputSpan<'a, ForwardedMessage<T, F>>,
}

impl<'a, T: Default, F> ForwardedOutputSpan<'a, T, F> {
    pub(crate) fn new(
        publisher: &'a mut ForwardingPublisher<T, F>,
        forwarded_ptrs: impl IntoIterator<Item = ArenaReaderPtr<Message<F>>>,
    ) -> Self {
        let mut ptrs = forwarded_ptrs.into_iter();
        ForwardedOutputSpan {
            inner: OutputSpan::new_with_factory(&mut publisher.inner, |slot| {
                let ptr = ptrs
                    .next()
                    .expect("not enough forwarded ptrs for span capacity");
                slot.write(ForwardedMessage::new_with_forward(ptr));
            }),
        }
    }

    pub fn outputs(&self) -> impl Iterator<Item = &T> {
        self.inner.outputs().map(|fwd| &fwd.message)
    }

    pub fn outputs_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.inner.outputs_mut().map(|fwd| &mut fwd.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
}
