use crate::callback::CallbackReadiness;
use crate::double_buffer::{DoubleBuffer, ReadBufferGuard, WriteBufferHandle};
use crate::generic_subscriber;
pub use crate::generic_subscriber::GenericSubscriber;
use crate::message::Message;
use crate::pub_sub::ChannelName;
use std::sync::Arc;

pub struct PublishError {}

#[derive(Clone)]
pub struct SubscriberConfig {
    pub is_optional: bool,
    pub capacity: usize,
    pub is_trigger: bool,
    /// Whether to keep elements across runs. Requires the user to explicitly consume data
    pub keep_across_runs: bool,

    pub channel_name: ChannelName,
}

#[allow(dead_code)]
pub struct Subscriber<T> {
    buffers: DoubleBuffer<Message<T>>,
    writer_queue_drops: usize,
    reader_queue_drops: usize,
    queue_has_new_data: bool,
    config: SubscriberConfig,
    readiness_state: Option<(Arc<CallbackReadiness>, usize)>,
}

impl<T> Subscriber<T> {
    pub fn new(config: SubscriberConfig) -> Self {
        Subscriber {
            buffers: DoubleBuffer::new(config.capacity),
            config,
            writer_queue_drops: 0,
            reader_queue_drops: 0,
            queue_has_new_data: false,
            readiness_state: None,
        }
    }

    pub fn get_config(&self) -> &SubscriberConfig {
        &self.config
    }

    pub fn get_writer_queue_drops(&self) -> usize {
        self.writer_queue_drops
    }

    pub fn get_reader_queue_drops(&self) -> usize {
        self.reader_queue_drops
    }

    pub fn get_write_guard(&mut self) -> WriteBufferHandle<Message<T>> {
        self.buffers.get_write_buffer()
    }

    pub fn drain_writer_to_reader(&self) {
        self.buffers.drain_writer_to_reader();
        // Clear our bit — write buffer is now empty
        if let Some((readiness, index)) = &self.readiness_state {
            readiness.clear_bit(*index);
        }
    }

    pub fn get_read_buffer<'a>(&'a self) -> ReadBufferGuard<'a, Message<T>> {
        self.buffers.get_read_buffer()
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
            !self.buffers.get_read_buffer().is_empty()
        }
    }

    fn get_config(&self) -> &SubscriberConfig {
        &self.config
    }

    fn get_config_mut(&mut self) -> &mut SubscriberConfig {
        &mut self.config
    }

    fn requests_execution(&self) -> bool {
        if self.config.is_trigger {
            !self.buffers.get_write_buffer().is_empty()
        } else {
            false
        }
    }

    fn drain_writer_to_reader(&self) {
        Subscriber::drain_writer_to_reader(self);
    }

    fn get_queue_info(&self) -> generic_subscriber::QueueInfo {
        generic_subscriber::QueueInfo {
            reader_size: self.buffers.get_read_buffer().len(),
            writer_size: self.buffers.get_write_buffer().len(),
        }
    }

    fn cleanup_buffers(&self) {
        self.buffers.clear();
    }

    fn set_readiness_state(&mut self, state: Arc<CallbackReadiness>, bit_index: usize) {
        self.readiness_state = Some((state, bit_index));
    }

    fn get_readiness_state(&self) -> Option<(Arc<CallbackReadiness>, usize)> {
        self.readiness_state.clone()
    }

    fn for_each_queued_input(&self, f: &mut dyn FnMut(&dyn std::any::Any)) {
        let mut guard = self.buffers.get_read_buffer();
        for message_ptr in guard.as_slice() {
            f(message_ptr as &dyn std::any::Any);
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

    fn get_config(&self) -> &SubscriberConfig {
        self.subscriber.get_config()
    }

    fn get_config_mut(&mut self) -> &mut SubscriberConfig {
        self.subscriber.get_config_mut()
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

    fn get_queue_info(&self) -> generic_subscriber::QueueInfo {
        GenericSubscriber::get_queue_info(&self.subscriber)
    }

    fn cleanup_buffers(&self) {
        self.subscriber.cleanup_buffers();
    }

    fn set_readiness_state(&mut self, state: Arc<CallbackReadiness>, bit_index: usize) {
        self.subscriber.set_readiness_state(state, bit_index);
    }

    fn get_readiness_state(&self) -> Option<(Arc<CallbackReadiness>, usize)> {
        self.subscriber.get_readiness_state()
    }
}
