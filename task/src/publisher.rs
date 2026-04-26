use crate::arena::{Arena, ArenaPtr};
use crate::callback::CallbackReadiness;
use crate::double_buffer::WriteBufferHandle;
use crate::generic_publisher::ConnectionTypeMismatch;
pub use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::message::{Message, MessageHeader};
use crate::pub_sub::ChannelName;
use crate::subscriber::{Subscriber, SubscriberConfig};
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

struct SubscriberBuffer<T> {
    buffer: WriteBufferHandle<ArenaPtr<Message<T>>>,
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
        for mut loaned_value in &mut self.loaned_values.drain(..) {
            if loaned_value.sent {
                let header = MessageHeader {
                    published_at: timestamp,
                };
                // SAFETY: For a loaned value to have been sent, it must have been initialized
                unsafe {
                    let maybe_uninit_payload = loaned_value.ptr.payload.get_mut();
                    maybe_uninit_payload.assume_init_mut().header = header;
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
        unsafe {
            // SAFETY: Publisher guarantees that the value has been initialized on loan
            // and a loaned value is exclusive acifcess.
            &(*self
                .publisher
                .loaned_value_at(self.loaned_value_idx)
                .ptr
                .payload
                .get())
            .assume_init_ref()
            .message
        }
    }
}

impl<'a, T> DerefMut for Output<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            // SAFETY: Publisher guarantees that the value has been initialized on loan
            // and a loaned value is exclusive access.
            &mut (*self
                .publisher
                .loaned_value_at_mut(self.loaned_value_idx)
                .ptr
                .payload
                .get())
            .assume_init_mut()
            .message
        }
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
}

pub struct OutputSpan<'a, T: Default> {
    // TODO use some fixed size vec deque
    loaned_value_idx_start: usize,
    loaned_value_idx_end: usize,
    publisher: &'a mut Publisher<T>,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscriber::Subscriber;

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

        publisher.flush_loaned_values(crate::time::FrameworkTime::from_wall_clock());

        assert!(subscriber.get_queue_info().writer_size == 1);
        assert!(subscriber.get_queue_info().reader_size == 0);

        assert!(subscriber.requests_execution());

        subscriber.drain_writer_to_reader();

        assert!(subscriber.able_to_run());

        assert!(subscriber.get_queue_info().writer_size == 0);
        assert!(subscriber.get_queue_info().reader_size == 1);
    }
}
