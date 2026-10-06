use crate::forwarded_message::ForwardedMessage;
use crate::generic_publisher::GenericPublisher;
use crate::message::{Message, MessageHeader};
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
        let remaining_loans = publisher.config().capacity - publisher.loaned_count();
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

impl<'iter, 'publisher, T> OutputUninitSpanIterator<'iter, 'publisher, T> {
    pub fn new(
        span: &'iter mut OutputUninitSpan<'publisher, T>,
    ) -> OutputUninitSpanIterator<'iter, 'publisher, T> {
        Self {
            arena: &mut *span.arena,
            staging: &span.staging,
            remaining_loans: &mut span.remaining_loans,
        }
    }
}

impl<'iter, 'publisher, T> Iterator for OutputUninitSpanIterator<'iter, 'publisher, T> {
    type Item = OutputUninit<'iter, 'publisher, T>;

    fn next(&mut self) -> Option<Self::Item> {
        if *self.remaining_loans == 0 {
            return None;
        }

        let mut arena_ptr_uninit = self.arena.try_allocate_uninit()?;
        let msg_ptr = arena_ptr_uninit.payload_uninit().as_mut_ptr();
        // SAFETY: The reservation owns exclusive storage for Message<T>.
        // Initialize only the header without referencing the uninitialized payload.
        unsafe {
            (&raw mut (*msg_ptr).header).write(MessageHeader::default());
        }
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
    use crate::publisher::PublisherConfig;
    use crate::subscriber::{Subscriber, SubscriberConfig};
    use crate::time::FrameworkTime;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn publisher<T: Send + Sync + 'static>(capacity: usize) -> Publisher<T> {
        let mut publisher = Publisher::new(PublisherConfig {
            capacity,
            channel_name: "channel".into(),
        });
        publisher.allocate_arena();
        publisher
    }

    #[test]
    fn uninit_span_stops_at_budget_even_with_free_arena_slots() {
        for capacity in [0, 2] {
            let mut publisher = publisher::<u64>(capacity);
            publisher.increase_arena_size(3);
            publisher.allocate_arena();

            let mut span = OutputUninitSpan::new(&mut publisher);
            let mut outputs = OutputUninitSpanIterator::new(&mut span);
            for _ in 0..capacity {
                // Abandoning an output releases storage, but consumes an iteration item.
                drop(
                    outputs
                        .next()
                        .expect("configured output should be available"),
                );
            }
            assert!(outputs.next().is_none());
            assert!(outputs.next().is_none());
        }
    }

    #[test]
    fn uninit_span_supports_live_outputs_after_iterator_drop_and_publishes_in_send_order() {
        struct Packet {
            sequence: u64,
            words: [u32; 4],
        }

        let mut publisher = publisher::<Packet>(3);
        let mut subscriber = Subscriber::new(SubscriberConfig {
            is_optional: false,
            capacity: 3,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: "uninit_span".into(),
        });
        publisher.add_typed_subscriber(&mut subscriber);
        publisher.allocate_arena();

        {
            let mut span = OutputUninitSpan::new(&mut publisher);
            let mut outputs: Vec<_> = OutputUninitSpanIterator::new(&mut span).collect();
            assert_eq!(outputs.len(), 3);

            for (index, output) in outputs.iter_mut().enumerate() {
                let sequence = index as u64 + 1;
                let ptr = output.value_uninit().as_mut_ptr();
                // SAFETY: Each output exclusively owns its reservation. These raw
                // field writes fully initialize Packet without constructing a &mut Packet.
                unsafe {
                    (&raw mut (*ptr).sequence).write(sequence);
                    (&raw mut (*ptr).words).write([sequence as u32; 4]);
                }
            }

            for output in outputs.into_iter().rev() {
                // SAFETY: Both fields of every Packet were initialized above.
                unsafe { output.send_assume_init() };
            }
        }

        assert_eq!(publisher.loaned_count(), 3);
        subscriber.drain_writer_to_reader();
        assert!(subscriber.read_buffer().is_empty());

        let timestamp = FrameworkTime::from_nanoseconds(42);
        publisher.flush_loaned_values(timestamp);
        assert_eq!(publisher.loaned_count(), 0);
        subscriber.drain_writer_to_reader();
        {
            let mut messages = subscriber.read_buffer();
            assert_eq!(messages.len(), 3);
            for (message, sequence) in messages.as_slice().zip([3, 2, 1]) {
                assert_eq!(message.header.published_at, timestamp);
                assert_eq!(message.message.sequence, sequence);
                assert_eq!(message.message.words, [sequence as u32; 4]);
            }
        }
        subscriber.cleanup_buffers();
    }

    struct TrackedPayload {
        value: u64,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for TrackedPayload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn uninit_span_abandonment_releases_reserved_and_unrequested_capacity() {
        let mut publisher = publisher::<TrackedPayload>(2);
        {
            let mut span = OutputUninitSpan::new(&mut publisher);
            let mut output = OutputUninitSpanIterator::new(&mut span).next().unwrap();
            let ptr = output.value_uninit().as_mut_ptr();
            // SAFETY: The reservation owns this field. Leave the Arc field
            // uninitialized; abandoning the output must not run TrackedPayload::drop.
            unsafe { (&raw mut (*ptr).value).write(99) };
        }
        assert_eq!(publisher.loaned_count(), 0);

        let drops = Arc::new(AtomicUsize::new(0));
        {
            let mut span = OutputUninitSpan::new(&mut publisher);
            let outputs: Vec<_> = OutputUninitSpanIterator::new(&mut span).collect();
            assert_eq!(outputs.len(), 2);
            for mut output in outputs {
                output.value_uninit().write(TrackedPayload {
                    value: 42,
                    drops: Arc::clone(&drops),
                });
                // SAFETY: Both fields of TrackedPayload were written above.
                unsafe { output.send_assume_init() };
            }
        }
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        publisher.flush_loaned_values(FrameworkTime::from_nanoseconds(1));
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        drop(publisher);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn uninit_span_unwind_restores_sent_loans_and_releases_partial_reservations() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut publisher = publisher::<TrackedPayload>(2);
        let allocation = publisher.loaned_values_mut().as_ptr();
        let capacity = publisher.loaned_values_mut().capacity();

        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut span = OutputUninitSpan::new(&mut publisher);
            let mut outputs = OutputUninitSpanIterator::new(&mut span);
            let mut sent = outputs.next().unwrap();
            let mut partial = outputs.next().unwrap();

            sent.value_uninit().write(TrackedPayload {
                value: 7,
                drops: Arc::clone(&drops),
            });
            // SAFETY: Both fields of TrackedPayload were written above.
            unsafe { sent.send_assume_init() };

            let ptr = partial.value_uninit().as_mut_ptr();
            // SAFETY: The reservation owns this field. The Arc remains uninitialized.
            unsafe { (&raw mut (*ptr).value).write(99) };
            panic!("initialization failed");
        }));

        assert!(result.is_err());
        assert_eq!(publisher.loaned_count(), 1);
        assert_eq!(publisher.loaned_value_at(0).payload().value, 7);
        assert!(publisher.loaned_value_at(0).sent);
        assert_eq!(publisher.loaned_values_mut().as_ptr(), allocation);
        assert_eq!(publisher.loaned_values_mut().capacity(), capacity);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(
            publisher
                .loan_uninit()
                .expect("partial reservation was released"),
        );

        publisher.flush_loaned_values(FrameworkTime::from_nanoseconds(1));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(publisher);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn uninit_span_failed_arena_reservation_does_not_consume_budget() {
        let mut publisher = publisher::<u64>(1);
        let held = publisher.loan_uninit().unwrap();

        {
            let mut span = OutputUninitSpan::new(&mut publisher);
            assert!(OutputUninitSpanIterator::new(&mut span).next().is_none());
            assert_eq!(span.remaining_loans, 1);

            drop(held);
            drop(
                OutputUninitSpanIterator::new(&mut span)
                    .next()
                    .expect("released slot is available"),
            );
            assert_eq!(span.remaining_loans, 0);
        }
        assert_eq!(publisher.loaned_count(), 0);
        assert!(publisher.loan_uninit().is_ok());
    }
}
