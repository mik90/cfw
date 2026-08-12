use crate::callback::SubscriberReadiness;
use crate::forwarded_message::ForwardedMessage;
use crate::generic_publisher::ConnectionTypeMismatch;
pub use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::message::{Message, MessageHeader};
use crate::pub_sub::ChannelName;
use crate::subscriber::{ForwardableSubscriber, Subscriber, SubscriberConfig};
use crate::time::FrameworkTime;
use base::arena::{Arena, ArenaPtr, ArenaReaderPtr};
use base::double_buffer::WriteBufferHandle;
use std::any::Any;
use std::mem::MaybeUninit;

#[derive(Debug)]
pub enum LoanError {
    LoanCapacityReached,
}

#[derive(Debug)]
pub struct SendError;

pub struct PublisherConfig {
    pub capacity: usize,
    pub channel_name: ChannelName,
}

pub(crate) struct LoanedValue<T> {
    pub ptr: ArenaPtr<Message<T>>,
    pub sent: bool,
}

impl<T> LoanedValue<T> {
    fn new(ptr: ArenaPtr<Message<T>>) -> Self {
        LoanedValue { ptr, sent: false }
    }

    pub(crate) fn value(&self) -> &Message<T> {
        // SAFETY: For a loaned value to have been created, the message should have been initialized
        unsafe { (*self.ptr.payload.get()).assume_init_ref() }
    }

    pub(crate) fn value_mut(&mut self) -> &mut Message<T> {
        // SAFETY: For a loaned value to have been created, the message should have been initialized
        unsafe { (*self.ptr.payload.get()).assume_init_mut() }
    }

    pub(crate) fn payload(&self) -> &T {
        &self.value().message
    }

    /// Borrow the payload (`Message<T>::message`) of this loan mutably.
    pub(crate) fn payload_mut(&mut self) -> &mut T {
        &mut self.value_mut().message
    }

    pub(crate) fn header(&self) -> &MessageHeader {
        &self.value().header
    }
}

#[allow(dead_code)]
struct SubscriberBuffer<T> {
    buffer: WriteBufferHandle<Message<T>>,
    subscriber_config: SubscriberConfig,
    /// Readiness role for the target CallbackNode: a gating bit to set for
    /// required inputs, or a bit-less handle to nudge for optional+trigger
    /// inputs (set during connection).
    readiness: Option<SubscriberReadiness>,
}

pub struct Publisher<T> {
    config: PublisherConfig,
    /// Drop ordering is relevant here, arena must be dropped last since loaned values are pointers into the arena
    pub(crate) loaned_values: Vec<LoanedValue<T>>,
    subscriber_write_buffers: Vec<SubscriberBuffer<T>>,
    arena: Arena<Message<T>>,
    /// This _could_ be part of the publisher config but it's something tied to `T` so it's better to keep it outside of a
    /// user-configurable thing like publisher config (probably).
    forwarded_channels: Vec<ChannelName>,
}

impl<T: 'static> GenericPublisher for Publisher<T> {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn config(&self) -> &PublisherConfig {
        &self.config
    }

    fn config_mut(&mut self) -> &mut PublisherConfig {
        &mut self.config
    }

    fn forwarded_channels(&self) -> &[ChannelName] {
        self.forwarded_channels.as_slice()
    }

    fn flush_loaned_values(&mut self, timestamp: FrameworkTime) {
        self.flush_loaned_values_with(timestamp, &mut |_header| {});
    }

    fn flush_loaned_values_logged(
        &mut self,
        timestamp: FrameworkTime,
        hook: &mut dyn FnMut(&MessageHeader),
    ) {
        self.flush_loaned_values_with(timestamp, hook);
    }

    fn allocate_arena(&mut self) {
        self.arena.allocate_slots();
    }

    fn for_each_pending_output(&self, f: &mut dyn FnMut(&MessageHeader, &dyn Any)) {
        for loaned in self.loaned_values.iter().filter(|lv| lv.sent) {
            // SAFETY: Publisher guarantees the value has been initialized on loan.
            let value: &Message<T> = unsafe { (*loaned.ptr.payload.get()).assume_init_ref() };
            f(&value.header, &value.message as &dyn Any);
        }
    }

    fn increase_arena_size(&mut self, additional_capacity: usize) {
        let starting_capacity = self.arena.capacity();
        self.arena
            .update_capacity(starting_capacity + additional_capacity);
    }

    fn connect_to_subscriber(
        &mut self,
        subscriber: &mut dyn GenericSubscriber,
    ) -> Result<(), ConnectionTypeMismatch> {
        if let Some(typed) = subscriber
            .as_any()
            .downcast_mut::<crate::subscriber::Subscriber<T>>()
        {
            self.add_typed_subscriber(typed);
            return Ok(());
        }
        if let Some(typed) = subscriber
            .as_any()
            .downcast_mut::<ForwardableSubscriber<T>>()
        {
            self.add_typed_forwarded_subscriber(typed);
            return Ok(());
        }
        Err(ConnectionTypeMismatch {})
    }

    fn build_matching_subscriber(
        &self,
        config: SubscriberConfig,
    ) -> Option<Box<dyn GenericSubscriber>> {
        Some(Box::new(Subscriber::<T>::new(config)))
    }

    fn value_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<T>()
    }
}

impl<T> Publisher<T> {
    /// Shared flush implementation: stamp each sent loan with `timestamp`,
    /// fan it out to subscriber write buffers, and invoke `hook` with each
    /// published header. Used by both the plain and logged flush paths.
    fn flush_loaned_values_with(
        &mut self,
        timestamp: FrameworkTime,
        hook: &mut dyn FnMut(&MessageHeader),
    ) {
        for loaned_value in self.loaned_values.drain(..) {
            if loaned_value.sent {
                let header = MessageHeader {
                    published_at: timestamp,
                };
                // SAFETY: The loaned value was initialized on loan and `loaned_value` is
                // the only ArenaPtr to this slot at this point — clones haven't been
                // handed to subscribers yet (that happens in the loop below). Using
                // UnsafeCell::get() instead of DerefMut avoids creating an aliasing
                // &mut ArenaSlot<T>, which would be UB once clones exist.
                unsafe {
                    (*loaned_value.ptr.payload.get()).assume_init_mut().header = header;
                }

                hook(&header);

                for subscriber_buffer in &mut self.subscriber_write_buffers {
                    // Copy the arena pointer to each subscriber buffer
                    subscriber_buffer.buffer.write(loaned_value.ptr.clone());

                    // Notify the target CallbackNode's readiness: set the
                    // gating bit for required inputs, nudge the node for
                    // optional+trigger inputs (it enqueues if the required
                    // inputs are ready).
                    match &subscriber_buffer.readiness {
                        Some(SubscriberReadiness::Gating(readiness, bit_index)) => {
                            readiness.set_bit(*bit_index);
                        }
                        Some(SubscriberReadiness::OptionalTrigger(readiness)) => {
                            readiness.enqueue_if_ready();
                        }
                        None => {}
                    }
                }
            }
        }
    }

    pub fn new(config: PublisherConfig) -> Self {
        let capacity = config.capacity;
        Publisher {
            config,
            // Arena will be resized to allow for enough data for subscribers
            arena: Arena::new(capacity),
            subscriber_write_buffers: vec![],
            loaned_values: Vec::with_capacity(capacity),
            forwarded_channels: vec![],
        }
    }

    pub fn new_with_forwards(
        config: PublisherConfig,
        forwarded_channels: Vec<ChannelName>,
    ) -> Self {
        let capacity = config.capacity;
        Publisher {
            config,
            // Arena will be resized to allow for enough data for subscribers
            arena: Arena::new(capacity),
            subscriber_write_buffers: vec![],
            loaned_values: Vec::with_capacity(capacity),
            forwarded_channels,
        }
    }

    pub fn config(&self) -> &PublisherConfig {
        &self.config
    }

    pub(crate) fn loaned_count(&self) -> usize {
        self.loaned_values.len()
    }

    pub(crate) fn loaned_value_at(&self, index: usize) -> &LoanedValue<T> {
        &self.loaned_values[index]
    }

    pub(crate) fn loaned_value_at_mut(&mut self, index: usize) -> &mut LoanedValue<T> {
        &mut self.loaned_values[index]
    }

    /// Mutably borrow the payload (`Message<T>::message`) of an outstanding
    /// loan by index. Lets a long-lived loan be mutated in place across
    /// several writes before being sent — used by executors that fill an
    /// execution-log message over multiple executions before flushing it.
    pub fn loaned_payload_mut(&mut self, index: usize) -> &mut T {
        let msg: &mut Message<T> = self.loaned_value_at_mut(index).value_mut();
        &mut msg.message
    }

    /// Mark an outstanding loan as sent so a subsequent `flush_loaned_values`
    /// will publish it. Paired with [`loan_default`] / [`loaned_payload_mut`].
    pub fn mark_loan_sent(&mut self, index: usize) {
        self.loaned_value_at_mut(index).sent = true;
    }

    pub(crate) fn loaned_values_at(
        &self,
        start_index: usize,
        end_index: usize,
    ) -> &[LoanedValue<T>] {
        &self.loaned_values[start_index..=end_index]
    }

    pub(crate) fn loaned_values_at_mut(
        &mut self,
        start_index: usize,
        end_index: usize,
    ) -> &mut [LoanedValue<T>] {
        &mut self.loaned_values[start_index..=end_index]
    }

    pub(crate) fn loan_with(
        &mut self,
        factory: impl FnOnce(&mut MaybeUninit<T>),
    ) -> Result<usize, LoanError> {
        if self.loaned_values.len() >= self.config.capacity {
            return Err(LoanError::LoanCapacityReached);
        }
        let allocated_ptr = match self.arena.try_allocate_with(|slot| {
            let msg_ptr = slot.as_mut_ptr();
            // SAFETY: All fields of `Message<T>` are initialized before the slot is assumed init:
            // header is written here; factory is responsible for fully initializing `message`.
            unsafe {
                let header = std::ptr::addr_of_mut!((*msg_ptr).header);
                let message = std::ptr::addr_of_mut!((*msg_ptr).message).cast::<MaybeUninit<T>>();
                header.write(MessageHeader::default());
                factory(&mut *message);
            }
        }) {
            Some(ptr) => ptr,
            None => {
                panic!(
                    "Tried to publish on channel {}. Expected pub-sub system to allocate correct arena sizes but we used all {} slots!",
                    self.config.channel_name, self.arena.capacity().
                );
            }
        };
        self.loaned_values.push(LoanedValue::new(allocated_ptr));
        Ok(self.loaned_values.len() - 1)
    }

    // Loans cannot be held across runs
}

impl<T: 'static> Publisher<T> {
    pub fn add_typed_subscriber(&mut self, typed_subscriber: &mut Subscriber<T>) {
        let buffer_guard = typed_subscriber.write_guard();
        let config = typed_subscriber.config().clone();

        // Take over whatever readiness role the subscriber's node injected:
        // a gating bit for required inputs, a nudge handle for
        // optional+trigger inputs, or nothing for optional non-trigger
        // inputs (their data arrival never affects scheduling).
        let readiness = typed_subscriber.readiness_state();

        self.subscriber_write_buffers.push(SubscriberBuffer {
            buffer: buffer_guard,
            subscriber_config: config,
            readiness,
        });
        // Grow the arena to cover clones this subscriber may hold simultaneously:
        // up to `capacity` in its write queue plus up to `capacity` in its read
        // buffer (the previous message can still be live when the publisher
        // publishes again before the next drain). Without this, a back-to-back
        // publisher run exhausts the arena slots and panics — which, because
        // cleanup_buffers runs *after* thread joins, also surfaces as a
        // use-after-free under Miri when the panicked worker leaves ArenaPtrs
        // in the subscriber queue and the owning arena is dropped first.
        self.increase_arena_size(typed_subscriber.config().arena_footprint());
    }

    pub fn add_typed_forwarded_subscriber(
        &mut self,
        forwardable_subscriber: &mut ForwardableSubscriber<T>,
    ) {
        self.add_typed_subscriber(&mut forwardable_subscriber.subscriber)
    }

    pub fn loan_and_init(
        &mut self,
        initializer: impl FnOnce(&mut MaybeUninit<T>),
    ) -> Result<usize, LoanError> {
        self.loan_with(initializer)
    }
}

impl<T: Default> Publisher<T> {
    pub fn loan_default(&mut self) -> Result<usize, LoanError> {
        self.loan_with(|slot| {
            slot.write(T::default());
        })
    }
}

impl<T: Default + 'static, F: 'static> Publisher<ForwardedMessage<T, F>> {
    pub fn loan_forwarded(
        &mut self,
        forwarded_ptr: ArenaReaderPtr<Message<F>>,
    ) -> Result<usize, LoanError> {
        self.loan_with(|slot| {
            slot.write(ForwardedMessage::new_with_forward(forwarded_ptr));
        })
    }
}

pub struct ForwardingPublisher<T, F> {
    pub(crate) inner: Publisher<ForwardedMessage<T, F>>,
}

impl<T: Default + 'static, F: 'static> ForwardingPublisher<T, F> {
    pub fn new(config: PublisherConfig, forwarded_channels: Vec<ChannelName>) -> Self {
        Self {
            inner: Publisher::new_with_forwards(config, forwarded_channels),
        }
    }

    pub fn add_typed_subscriber(&mut self, subscriber: &mut Subscriber<ForwardedMessage<T, F>>) {
        self.inner.add_typed_subscriber(subscriber);
    }

    pub fn allocate_arena(&mut self) {
        self.inner.allocate_arena();
    }

    pub fn forwarded_channels(&self) -> &[ChannelName] {
        self.inner.forwarded_channels.as_slice()
    }

    pub fn flush_loaned_values(&mut self, timestamp: FrameworkTime) {
        GenericPublisher::flush_loaned_values(&mut self.inner, timestamp);
    }

    pub fn new_downcasted(publisher: &mut dyn GenericPublisher) -> &mut Self {
        publisher
            .as_any()
            .downcast_mut::<ForwardingPublisher<T, F>>()
            .expect("Expected proc macro to use the correct types")
    }
}

impl<T: Default + 'static, F: 'static> GenericPublisher for ForwardingPublisher<T, F> {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn config(&self) -> &PublisherConfig {
        self.inner.config()
    }

    fn config_mut(&mut self) -> &mut PublisherConfig {
        self.inner.config_mut()
    }

    fn forwarded_channels(&self) -> &[ChannelName] {
        GenericPublisher::forwarded_channels(&self.inner)
    }

    fn flush_loaned_values(&mut self, timestamp: FrameworkTime) {
        GenericPublisher::flush_loaned_values(&mut self.inner, timestamp);
    }

    fn flush_loaned_values_logged(
        &mut self,
        timestamp: FrameworkTime,
        hook: &mut dyn FnMut(&MessageHeader),
    ) {
        self.inner.flush_loaned_values_with(timestamp, hook);
    }

    fn allocate_arena(&mut self) {
        self.inner.allocate_arena();
    }

    fn increase_arena_size(&mut self, additional_capacity: usize) {
        self.inner.increase_arena_size(additional_capacity);
    }

    fn connect_to_subscriber(
        &mut self,
        subscriber: &mut dyn GenericSubscriber,
    ) -> Result<(), ConnectionTypeMismatch> {
        self.inner.connect_to_subscriber(subscriber)
    }

    fn build_matching_subscriber(
        &self,
        config: SubscriberConfig,
    ) -> Option<Box<dyn GenericSubscriber>> {
        Some(Box::new(Subscriber::<ForwardedMessage<T, F>>::new(config)))
    }

    fn value_type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<ForwardedMessage<T, F>>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Output;
    use crate::subscriber::Subscriber;
    use crate::time;

    #[test]
    fn one_allocation() {
        let mut publisher = Publisher::<i32>::new(PublisherConfig {
            capacity: 1,
            channel_name: "channel".into(),
        });
        publisher.allocate_arena();
        assert!(publisher.loan_default().is_ok());
        assert!(publisher.loan_default().is_err());
    }

    #[test]
    fn multi_allocation() {
        let config = PublisherConfig {
            capacity: 3,
            channel_name: "channel".into(),
        };
        let mut publisher = Publisher::<i32>::new(config);
        publisher.allocate_arena();
        assert!(publisher.loan_default().is_ok());
        assert!(publisher.loan_default().is_ok());
        assert!(publisher.loan_default().is_ok());
        assert!(publisher.loan_default().is_err());
    }

    #[test]
    fn send() {
        let mut publisher = Publisher::<i32>::new(PublisherConfig {
            capacity: 1,
            channel_name: "channel".into(),
        });
        publisher.allocate_arena();
        let mut output = Output::new_default(&mut publisher);
        *output = 42;
        output.send();
    }

    #[test]
    fn send_to_subscriber() {
        let mut publisher = Publisher::<i32>::new(PublisherConfig {
            capacity: 1,
            channel_name: "channel".into(),
        });

        let mut subscriber = Subscriber::<i32>::new(SubscriberConfig {
            is_optional: false,
            capacity: 1,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: "channel".into(),
        });
        publisher.add_typed_subscriber(&mut subscriber);
        publisher.allocate_arena();
        assert!(!subscriber.able_to_run());
        assert!(!subscriber.requests_execution());
        let mut output = Output::new_default(&mut publisher);
        *output = 42;
        output.send();

        publisher.flush_loaned_values(time::FrameworkTime::from_nanoseconds(99));

        assert!(subscriber.queue_info().writer_size == 1);
        assert!(subscriber.queue_info().reader_size == 0);

        assert!(subscriber.requests_execution());

        subscriber.drain_writer_to_reader();

        assert!(subscriber.able_to_run());

        assert!(subscriber.queue_info().writer_size == 0);
        assert!(subscriber.queue_info().reader_size == 1);

        let read_buffer = subscriber.read_buffer();
        assert_eq!(read_buffer.len(), 1);
        let front = read_buffer.front();
        assert!(front.is_some());
        let front_message = front.unwrap();
        assert_eq!(
            front_message.header.published_at,
            time::FrameworkTime::from_nanoseconds(99)
        );
        assert_eq!(front_message.message, 42);
    }

    #[test]
    fn default_allocation_of_header() {
        let mut publisher = Publisher::<i32>::new(PublisherConfig {
            capacity: 1,
            channel_name: "channel".into(),
        });
        publisher.allocate_arena();
        assert!(publisher.loan_default().is_ok());
        let value = publisher.loaned_value_at(0);
        let header = &value.value().header;
        assert_eq!(header.published_at, FrameworkTime::INVALID);
    }

    /// Arena capacity must cover the publisher's own loans (`config.capacity`)
    /// plus `2 * capacity` for each subscriber: one set held in the subscriber's
    /// write queue and one held in its read buffer (the previous message can
    /// still be live when the publisher publishes again before the next drain).
    /// Otherwise a back-to-back publisher run exhausts the arena and panics.
    #[test]
    fn add_typed_subscriber_sizes_arena_for_write_and_read_buffers() {
        let mut publisher = Publisher::<u32>::new(PublisherConfig {
            capacity: 2,
            channel_name: "ch".into(),
        });
        assert_eq!(
            publisher.arena.capacity(),
            2,
            "arena starts sized for the publisher's own loan capacity"
        );

        let mut subscriber = Subscriber::<u32>::new(SubscriberConfig {
            is_optional: false,
            capacity: 4,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: "ch".into(),
        });
        publisher.add_typed_subscriber(&mut subscriber);
        assert_eq!(
            publisher.arena.capacity(),
            2 + 2 * 4,
            "subscriber must bump arena by 2 * capacity (write + read buffers)"
        );

        let mut second_subscriber = Subscriber::<u32>::new(SubscriberConfig {
            is_optional: true,
            capacity: 3,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: "ch".into(),
        });
        publisher.add_typed_subscriber(&mut second_subscriber);
        assert_eq!(
            publisher.arena.capacity(),
            2 + 2 * 4 + 2 * 3,
            "each additional subscriber adds another 2 * capacity"
        );
    }

    #[test]
    fn back_to_back_publish_does_not_exhaust_arena() {
        const PUB_CAPACITY: usize = 1;
        const SUB_CAPACITY: usize = 1;

        let mut publisher = Publisher::<u32>::new(PublisherConfig {
            capacity: PUB_CAPACITY,
            channel_name: "ch".into(),
        });

        let mut subscriber = Subscriber::<u32>::new(SubscriberConfig {
            is_optional: false,
            capacity: SUB_CAPACITY,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: "ch".into(),
        });
        publisher.add_typed_subscriber(&mut subscriber);
        publisher.allocate_arena();
        // Corrected sizing: publisher's own loan capacity + 2 * subscriber
        // capacity (clone in write queue + clone in read buffer).
        assert_eq!(publisher.arena.capacity(), PUB_CAPACITY + 2 * SUB_CAPACITY);

        let time = time::FrameworkTime::from_nanoseconds(0);

        // Cycle 1: publish msg1 and drain it into the subscriber's read buffer.
        {
            let mut out = Output::new_default(&mut publisher);
            *out = 10;
            out.send();
        }
        publisher.flush_loaned_values(time);
        subscriber.drain_writer_to_reader();
        assert_eq!(subscriber.queue_info().reader_size, 1);

        // Cycle 2: publish msg2 *without* draining. The subscriber still holds
        // msg1 in its read buffer, so msg2 must occupy a different arena slot.
        {
            let mut out = Output::new_default(&mut publisher);
            *out = 20;
            out.send();
        }
        publisher.flush_loaned_values(time);
        assert_eq!(subscriber.queue_info().writer_size, 1);
        assert_eq!(subscriber.queue_info().reader_size, 1);

        // Cycle 3: publish msg3 *without* draining since cycle 2. The
        // subscriber simultaneously holds msg1 (read buffer) and msg2 (write
        // queue). With the old `1 * capacity` arena sizing (cap 2), this loan
        // would have no free slot and `Arena::allocate_with` would panic.
        {
            let mut out = Output::new_default(&mut publisher);
            *out = 30;
            out.send();
        }
        publisher.flush_loaned_values(time);
    }
}
