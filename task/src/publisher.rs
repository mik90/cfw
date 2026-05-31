use crate::arena::{Arena, ArenaPtr, ArenaReaderPtr};
use crate::callback::CallbackReadiness;
use crate::double_buffer::WriteBufferHandle;
use crate::forwarded_message::ForwardedMessage;
use crate::generic_publisher::ConnectionTypeMismatch;
pub use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::message::{Message, MessageHeader};
use crate::pub_sub::ChannelName;
use crate::subscriber::{ForwardableSubscriber, Subscriber, SubscriberConfig};
use crate::time::FrameworkTime;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

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

struct LoanedValue<T> {
    pub ptr: ArenaPtr<Message<T>>,
    pub sent: bool,
}

impl<T> LoanedValue<T> {
    fn new(ptr: ArenaPtr<Message<T>>) -> Self {
        LoanedValue { ptr, sent: false }
    }
}

impl<T> Deref for LoanedValue<T> {
    type Target = Message<T>;

    fn deref(&self) -> &Self::Target {
        // SAFETY: For a loaned value to have been created, the MessageHeader should have been initialized
        unsafe { (*self.ptr.payload.get()).assume_init_ref() }
    }
}

impl<T> DerefMut for LoanedValue<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: For a loaned value to have been created, the MessageHeader should have been initialized
        unsafe { (*self.ptr.payload.get()).assume_init_mut() }
    }
}

#[allow(dead_code)]
struct SubscriberBuffer<T> {
    buffer: WriteBufferHandle<Message<T>>,
    subscriber_config: SubscriberConfig,
    /// Readiness bitmask and bit index for the target ConnectedCallback, if the
    /// subscriber is a trigger+non-optional input (set during connection).
    readiness: Option<(Arc<CallbackReadiness>, usize)>,
}

pub struct Publisher<T> {
    config: PublisherConfig,
    /// Drop ordering is relevant here, arena must be dropped last since loaned values are pointers into the arena
    loaned_values: Vec<LoanedValue<T>>,
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

    fn get_config(&self) -> &PublisherConfig {
        &self.config
    }

    fn get_config_mut(&mut self) -> &mut PublisherConfig {
        &mut self.config
    }

    fn get_forwarded_channels(&self) -> &[ChannelName] {
        self.forwarded_channels.as_slice()
    }

    fn flush_loaned_values(&mut self, timestamp: FrameworkTime) {
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

                for subscriber_buffer in &mut self.subscriber_write_buffers {
                    // Copy the arena pointer to each subscriber buffer
                    subscriber_buffer.buffer.write(loaned_value.ptr.clone());

                    // Notify the target ConnectedCallback's readiness bitmask
                    if let Some((readiness, bit_index)) = &subscriber_buffer.readiness {
                        readiness.set_bit(*bit_index);
                    }
                }
            }
        }
    }

    fn for_each_pending_output(&self, f: &mut dyn FnMut(&dyn std::any::Any)) {
        for loaned in self.loaned_values.iter().filter(|lv| lv.sent) {
            // SAFETY: Publisher guarantees the value has been initialized on loan.
            let value: &Message<T> = unsafe { (*loaned.ptr.payload.get()).assume_init_ref() };
            f(value as &dyn std::any::Any);
        }
    }

    fn allocate_arena(&mut self) {
        self.arena.allocate_slots();
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
        match subscriber
            .as_any()
            .downcast_mut::<crate::subscriber::Subscriber<T>>()
        {
            Some(typed_subscriber) => {
                self.add_typed_subscriber(typed_subscriber);
                Ok(())
            }
            None => Err(ConnectionTypeMismatch {}),
        }
    }
}

impl<T> Publisher<T> {
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

    pub fn get_config(&self) -> &PublisherConfig {
        &self.config
    }

    fn loaned_value_at(&self, index: usize) -> &LoanedValue<T> {
        &self.loaned_values[index]
    }

    fn loaned_value_at_mut(&mut self, index: usize) -> &mut LoanedValue<T> {
        &mut self.loaned_values[index]
    }

    fn loaned_values_at(&self, start_index: usize, end_index: usize) -> &[LoanedValue<T>] {
        &self.loaned_values[start_index..=end_index]
    }

    fn loaned_values_at_mut(
        &mut self,
        start_index: usize,
        end_index: usize,
    ) -> &mut [LoanedValue<T>] {
        &mut self.loaned_values[start_index..=end_index]
    }

    pub(self) fn loan_with(
        &mut self,
        factory: impl FnOnce() -> Message<T>,
    ) -> Result<usize, LoanError> {
        if self.loaned_values.len() >= self.config.capacity {
            return Err(LoanError::LoanCapacityReached);
        }
        let allocated_ptr = self.arena.allocate_with(factory);
        self.loaned_values.push(LoanedValue::new(allocated_ptr));
        Ok(self.loaned_values.len() - 1)
    }

    // Loans cannot be held across runs
}

impl<T: 'static> Publisher<T> {
    pub fn add_typed_subscriber(&mut self, typed_subscriber: &mut Subscriber<T>) {
        let buffer_guard = typed_subscriber.get_write_guard();
        let config = typed_subscriber.get_config().clone();

        // Only track readiness for trigger+non-optional subscribers — those are the
        // ones whose bits start at 0 in the bitmask and must be set before enqueueing.
        let readiness = if config.is_trigger && !config.is_optional {
            typed_subscriber.get_readiness_state()
        } else {
            None
        };

        self.subscriber_write_buffers.push(SubscriberBuffer {
            buffer: buffer_guard,
            subscriber_config: config,
            readiness,
        });
        self.increase_arena_size(typed_subscriber.get_config().capacity);
    }

    pub fn add_typed_forwarded_subscriber(
        &mut self,
        forwardable_subscriber: &mut ForwardableSubscriber<T>,
    ) {
        self.add_typed_subscriber(&mut forwardable_subscriber.subscriber)
    }
}

impl<T: Default> Publisher<T> {
    // TODO impl loan_default and non-default mechanisms in case the underlying type is default-constructible
    pub fn loan(&mut self) -> Result<usize, LoanError> {
        if self.loaned_values.len() >= self.config.capacity {
            return Err(LoanError::LoanCapacityReached);
        }
        let allocated_ptr = self.arena.allocate_default();
        self.loaned_values.push(LoanedValue::new(allocated_ptr));

        Ok(self.loaned_values.len() - 1)
    }
}

#[allow(dead_code)]
pub struct PublishFailureCallback(Arc<Mutex<dyn FnMut(SendError)>>);

impl PublishFailureCallback {
    pub fn new<F>(f: F) -> Self
    where
        F: FnMut(SendError) + 'static,
    {
        PublishFailureCallback(Arc::new(Mutex::new(f)))
    }

    pub fn panic() -> Self {
        PublishFailureCallback(Arc::new(Mutex::new(|e| {
            panic!("Publish failed: {:?}", e);
        })))
    }
}

pub struct Output<'a, T> {
    publisher: &'a mut Publisher<T>,
    loaned_value_idx: usize,

    pub on_publish_failure: PublishFailureCallback,
}

impl<'a, T> Deref for Output<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self
            .publisher
            .loaned_value_at(self.loaned_value_idx)
            .message
    }
}

impl<'a, T> DerefMut for Output<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self
            .publisher
            .loaned_value_at_mut(self.loaned_value_idx)
            .message
    }
}

impl<'a, T: Default + 'static> Output<'a, T> {
    pub fn new_default(publisher: &'a mut Publisher<T>) -> Self {
        let loaned_value_idx = publisher
            .loan()
            .expect("We expect loans to always be available");

        Output {
            // TODO configurable loan failure callback which could prevent running?
            publisher,
            loaned_value_idx,
            on_publish_failure: PublishFailureCallback::panic(),
        }
    }

    pub fn new_downcasted(publisher: &mut dyn GenericPublisher) -> Output<'_, T> {
        let typed_publisher = publisher.as_any().downcast_mut::<Publisher<T>>();
        Output::new_default(typed_publisher.expect("Expected proc macro to use the correct types"))
    }
}

impl<'a, T> Output<'a, T> {
    pub fn send(self) {
        self.publisher
            .loaned_value_at_mut(self.loaned_value_idx)
            .sent = true;
    }

    pub(self) fn new_with_factory(
        publisher: &'a mut Publisher<T>,
        factory: impl FnOnce() -> Message<T>,
    ) -> Self {
        let loaned_value_idx = publisher
            .loan_with(factory)
            .expect("We expect loans to always be available");
        Output {
            publisher,
            loaned_value_idx,
            on_publish_failure: PublishFailureCallback::panic(),
        }
    }
}

pub struct OutputSpan<'a, T> {
    // TODO use some fixed size vec deque
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
                // SAFETY: Publisher guarantees that the value has been initialized on loan
                // and a loaned value is exclusive access.
                unsafe { &(*loaned_value.ptr.payload.get()).assume_init_ref().message })
    }

    pub fn outputs_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.publisher
            .loaned_values_at_mut(self.loaned_value_idx_start, self.loaned_value_idx_end)
            .iter_mut()
            .map(|loaned_value|
                // SAFETY: Publisher guarantees that the value has been initialized on loan
                // and a loaned value is exclusive access.
                unsafe { &mut (*loaned_value.ptr.payload.get()).assume_init_mut().message })
    }

    pub(self) fn new_with_factory(
        publisher: &'a mut Publisher<T>,
        mut factory: impl FnMut() -> Message<T>,
    ) -> Self {
        let count = publisher.get_config().capacity;
        let start = publisher.loaned_values.len();
        for _ in 0..count {
            publisher.loan_with(|| factory()).unwrap();
        }
        OutputSpan {
            loaned_value_idx_start: start,
            loaned_value_idx_end: start + count - 1,
            publisher,
        }
    }
}

impl<'a, T: Default + 'static> OutputSpan<'a, T> {
    pub fn new(publisher: &'a mut Publisher<T>) -> Self {
        for _ in 0..publisher.get_config().capacity {
            publisher.loan().unwrap();
        }
        OutputSpan {
            loaned_value_idx_start: 0,
            loaned_value_idx_end: publisher.get_config().capacity - 1,
            publisher,
        }
    }

    pub fn new_downcasted(publisher: &'a mut dyn GenericPublisher) -> OutputSpan<'a, T> {
        let typed_publisher = publisher.as_any().downcast_mut::<Publisher<T>>();
        OutputSpan::new(typed_publisher.expect("Expected proc macro to use the correct types"))
    }
}

pub struct ForwardingPublisher<T, F> {
    inner: Publisher<ForwardedMessage<T, F>>,
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

    pub fn get_forwarded_channels(&self) -> &[ChannelName] {
        self.inner.forwarded_channels.as_slice()
    }

    pub fn flush_loaned_values(&mut self, timestamp: FrameworkTime) {
        GenericPublisher::flush_loaned_values(&mut self.inner, timestamp);
    }
}

fn forwarded_message_factory<T: Default, F>(
    forwarded_ptr: ArenaReaderPtr<Message<F>>,
) -> impl FnOnce() -> Message<ForwardedMessage<T, F>> {
    || Message {
        header: forwarded_ptr.header.clone(),
        message: ForwardedMessage::new_with_forward(forwarded_ptr),
    }
}

pub struct ForwardingOutput<'a, T, F> {
    inner: Output<'a, ForwardedMessage<T, F>>,
}

impl<'a, T: Default + 'static, F: 'static> ForwardingOutput<'a, T, F> {
    pub fn new(
        publisher: &'a mut ForwardingPublisher<T, F>,
        forwarded_ptr: ArenaReaderPtr<Message<F>>,
    ) -> Self {
        ForwardingOutput {
            inner: Output::new_with_factory(
                &mut publisher.inner,
                forwarded_message_factory(forwarded_ptr),
            ),
        }
    }

    pub fn send(self) {
        self.inner.send();
    }
}

impl<T, F> Deref for ForwardingOutput<'_, T, F> {
    type Target = T;

    fn deref(&self) -> &T {
        &(*self.inner).message
    }
}

impl<T, F> DerefMut for ForwardingOutput<'_, T, F> {
    fn deref_mut(&mut self) -> &mut T {
        &mut (*self.inner).message
    }
}

pub struct ForwardingOutputSpan<'a, T, F> {
    inner: OutputSpan<'a, ForwardedMessage<T, F>>,
}

impl<'a, T: Default + 'static, F: 'static> ForwardingOutputSpan<'a, T, F> {
    pub fn new(
        publisher: &'a mut ForwardingPublisher<T, F>,
        forwarded_ptrs: impl IntoIterator<Item = ArenaReaderPtr<Message<F>>>,
    ) -> Self {
        let mut ptrs = forwarded_ptrs.into_iter();
        ForwardingOutputSpan {
            inner: OutputSpan::new_with_factory(&mut publisher.inner, || {
                let ptr = ptrs
                    .next()
                    .expect("not enough forwarded ptrs for span capacity");
                Message {
                    header: MessageHeader::default(),
                    message: ForwardedMessage::new_with_forward(ptr),
                }
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
    use crate::{subscriber::Subscriber, time};

    #[test]
    fn one_allocation() {
        let mut publisher = Publisher::<i32>::new(PublisherConfig {
            capacity: 1,
            channel_name: "channel".into(),
        });
        publisher.allocate_arena();
        assert!(publisher.loan().is_ok());
        assert!(publisher.loan().is_err());
    }

    #[test]
    fn multi_allocation() {
        let config = PublisherConfig {
            capacity: 3,
            channel_name: "channel".into(),
        };
        let mut publisher = Publisher::<i32>::new(config);
        publisher.allocate_arena();
        assert!(publisher.loan().is_ok());
        assert!(publisher.loan().is_ok());
        assert!(publisher.loan().is_ok());
        assert!(publisher.loan().is_err());
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

        assert!(subscriber.get_queue_info().writer_size == 1);
        assert!(subscriber.get_queue_info().reader_size == 0);

        assert!(subscriber.requests_execution());

        subscriber.drain_writer_to_reader();

        assert!(subscriber.able_to_run());

        assert!(subscriber.get_queue_info().writer_size == 0);
        assert!(subscriber.get_queue_info().reader_size == 1);

        let read_buffer = subscriber.get_read_buffer();
        assert_eq!(read_buffer.len(), 1);
        let front = read_buffer.front();
        assert!(front.is_some());
        let front_message = front.unwrap();
        assert_eq!(
            (*front_message).header.published_at,
            time::FrameworkTime::from_nanoseconds(99)
        );
        assert_eq!((*front_message).message, 42);
    }

    #[test]
    fn default_allocation_of_header() {
        let mut publisher = Publisher::<i32>::new(PublisherConfig {
            capacity: 1,
            channel_name: "channel".into(),
        });
        publisher.allocate_arena();
        assert!(publisher.loan().is_ok());
        let value = publisher.loaned_value_at(0);
        let header = &value.header;
        assert_eq!(header.published_at, FrameworkTime::INVALID);
    }
}
