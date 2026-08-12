use crate::callback::SubscriberReadiness;
use crate::generic_subscriber;
pub use crate::generic_subscriber::GenericSubscriber;
use crate::message::{Message, MessageHeader};
use crate::pub_sub::ChannelName;
use base::double_buffer::{DoubleBuffer, ReadBufferGuard, WriteBufferHandle};

pub struct PublishError {}

#[derive(Clone)]
pub struct SubscriberConfig {
    pub is_optional: bool,
    // Capacity of the read buffer.
    // Write buffer capacity is the same as this value.
    pub capacity: usize,
    pub is_trigger: bool,
    /// Whether to keep elements across runs. Requires the user to explicitly consume data
    pub keep_across_runs: bool,

    pub channel_name: ChannelName,
}

impl SubscriberConfig {
    /// Arena slots a publisher must reserve per connected subscriber: up to
    /// `capacity` in the write queue, up to `capacity` in the read buffer,
    /// and one for the pointer in flight between the two queues during a
    /// drain. See `Publisher::add_typed_subscriber` and
    /// `callback::find_forwarded_channel_usage`.
    pub fn arena_footprint(&self) -> usize {
        2 * self.capacity + 1
    }
}

#[allow(dead_code)]
pub struct Subscriber<T> {
    buffers: DoubleBuffer<Message<T>>,
    queue_has_new_data: bool,
    config: SubscriberConfig,
    readiness_state: Option<SubscriberReadiness>,
}

impl<T> Subscriber<T> {
    pub fn new(config: SubscriberConfig) -> Self {
        Subscriber {
            buffers: DoubleBuffer::new(config.capacity),
            config,
            queue_has_new_data: false,
            readiness_state: None,
        }
    }

    pub fn config(&self) -> &SubscriberConfig {
        &self.config
    }

    /// How many messages have been displaced from the write queue (due to overflow —
    /// the consumer didn't drain often enough to keep up) since creation.
    pub fn writer_queue_drops(&self) -> usize {
        self.buffers.writer_drops()
    }

    /// How many messages have been displaced from the read buffer (due to overflow —
    /// more arrived than `capacity` allows before being drained) since creation.
    pub fn reader_queue_drops(&self) -> usize {
        self.buffers.read_buffer().drops()
    }

    pub fn write_guard(&mut self) -> WriteBufferHandle<Message<T>> {
        self.buffers.write_buffer()
    }

    pub fn drain_writer_to_reader(&self) {
        self.buffers.drain_writer_to_reader();
        // A trigger input's bit means "new event pending": the imminent
        // run consumes it, so clear — re-firing requires new data.
        // A non-trigger required input's bit means "has a value": keep it
        // set while the read buffer retains one, so the node stays
        // runnable whenever a trigger fires; clear only once empty.
        // Optional-trigger subscribers own no bit — nothing to clear.
        if let Some(SubscriberReadiness::Gating(readiness, index)) = &self.readiness_state
            && (self.config.is_trigger || self.buffers.read_buffer().is_empty())
        {
            readiness.clear_bit(*index);
        }
    }

    pub fn read_buffer<'a>(&'a self) -> ReadBufferGuard<'a, Message<T>> {
        self.buffers.read_buffer()
    }

    /// Clear all buffered values. Should be called before the Arena is dropped
    /// to prevent ArenaPtrs from outliving their Arena.
    pub fn cleanup_buffers(&self) {
        self.buffers.clear();
    }
}

impl<T: 'static> GenericSubscriber for Subscriber<T> {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn able_to_run(&self) -> bool {
        if self.config.is_optional {
            true
        } else {
            !self.buffers.read_buffer().is_empty()
        }
    }

    fn config(&self) -> &SubscriberConfig {
        &self.config
    }

    fn config_mut(&mut self) -> &mut SubscriberConfig {
        &mut self.config
    }

    fn requests_execution(&self) -> bool {
        if self.config.is_trigger {
            !self.buffers.write_buffer().is_empty()
        } else {
            false
        }
    }

    fn drain_writer_to_reader(&self) {
        Subscriber::drain_writer_to_reader(self);
    }

    fn queue_info(&self) -> generic_subscriber::QueueInfo {
        generic_subscriber::QueueInfo {
            reader_size: self.buffers.read_buffer().len(),
            writer_size: self.buffers.write_buffer().len(),
        }
    }

    fn cleanup_buffers(&self) {
        self.buffers.clear();
    }

    fn set_readiness_state(&mut self, state: SubscriberReadiness) {
        self.readiness_state = Some(state);
    }

    fn readiness_state(&self) -> Option<SubscriberReadiness> {
        self.readiness_state.clone()
    }

    fn for_each_queued_input(&self, f: &mut dyn FnMut(&MessageHeader, &dyn std::any::Any)) {
        let mut guard = self.buffers.read_buffer();
        for message_ptr in guard.as_slice() {
            f(
                &message_ptr.header,
                &message_ptr.message as &dyn std::any::Any,
            );
        }
    }
}

pub struct ForwardableSubscriber<T> {
    pub subscriber: Subscriber<T>,
}

impl<T> ForwardableSubscriber<T> {
    pub fn new(config: SubscriberConfig) -> Self {
        Self {
            subscriber: Subscriber::new(config),
        }
    }
}

impl<T: 'static> GenericSubscriber for ForwardableSubscriber<T> {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn config(&self) -> &SubscriberConfig {
        self.subscriber.config()
    }

    fn config_mut(&mut self) -> &mut SubscriberConfig {
        self.subscriber.config_mut()
    }

    fn able_to_run(&self) -> bool {
        GenericSubscriber::able_to_run(&self.subscriber)
    }

    fn requests_execution(&self) -> bool {
        GenericSubscriber::requests_execution(&self.subscriber)
    }

    fn drain_writer_to_reader(&self) {
        self.subscriber.drain_writer_to_reader();
    }

    fn queue_info(&self) -> generic_subscriber::QueueInfo {
        GenericSubscriber::queue_info(&self.subscriber)
    }

    fn cleanup_buffers(&self) {
        self.subscriber.cleanup_buffers();
    }

    fn set_readiness_state(&mut self, state: SubscriberReadiness) {
        self.subscriber.set_readiness_state(state);
    }

    fn readiness_state(&self) -> Option<SubscriberReadiness> {
        self.subscriber.readiness_state()
    }

    fn for_each_queued_input(&self, f: &mut dyn FnMut(&MessageHeader, &dyn std::any::Any)) {
        self.subscriber.for_each_queued_input(f);
    }
}
