use crate::arena::ArenaPtr;
use crate::callback::CallbackReadiness;
use crate::double_buffer::{DoubleBuffer, ReadBufferGuard, WriteBufferHandle};
use crate::generic_subscriber;
pub use crate::generic_subscriber::GenericSubscriber;
use crate::message::Message;
use crate::pub_sub::ChannelName;
use std::ops::Deref;
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

pub struct Subscriber<T> {
    buffers: DoubleBuffer<ArenaPtr<Message<T>>>,
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

    pub fn get_write_guard(&mut self) -> WriteBufferHandle<ArenaPtr<Message<T>>> {
        self.buffers.get_write_buffer()
    }

    pub fn drain_writer_to_reader(&self) {
        self.buffers.drain_writer_to_reader();
        // Clear our bit — write buffer is now empty
        if let Some((readiness, index)) = &self.readiness_state {
            readiness.clear_bit(*index);
        }
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
            // SAFETY: Publisher guarantees that the value has been initialized and
            // any value in a subscriber queue cannot be modified by a publisher.
            let value: &Message<T> = unsafe { (*message_ptr.payload.get()).assume_init_ref() };
            f(value as &dyn std::any::Any);
        }
    }
}

pub struct RequiredInput<'a, T> {
    storage: ReadBufferGuard<'a, ArenaPtr<Message<T>>>,
}

impl<'a, T: 'static> RequiredInput<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> RequiredInput<'a, T> {
        RequiredInput {
            storage: subscriber.buffers.get_read_buffer(),
        }
    }
    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> RequiredInput<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        RequiredInput::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn value(&self) -> &T {
        self.deref()
    }
}

impl<T> Deref for RequiredInput<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: Publisher guarantees that the value has been initialized and
        // any value in a subscriber queue cannot be modified by a publisher.
        unsafe {
            &(*self
                .storage
                .front()
                .expect("Required input storage should have been validated before construction")
                .payload
                .get())
            .assume_init_ref()
            .message
        }
    }
}

pub struct OptionalInput<'a, T> {
    storage: ReadBufferGuard<'a, ArenaPtr<Message<T>>>,
    is_cleared: bool,
}

impl<'a, T: 'static> OptionalInput<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> OptionalInput<'a, T> {
        OptionalInput {
            storage: subscriber.buffers.get_read_buffer(),
            is_cleared: false,
        }
    }
    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> OptionalInput<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        OptionalInput::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn value(&'a self) -> Option<&'a T> {
        if self.is_cleared {
            // If the user has already cleared the input, then we can't let them clear the next one
            None
        } else {
            self.storage.front().map(| ptr|
                    // SAFETY: Publisher guarantees that the value has been initialized and
                    // any value in a subscriber queue cannot be modified by a publisher.
                    unsafe { &(*ptr.payload.get()).assume_init_ref().message })
        }
    }

    pub fn clear(&'a mut self) {
        self.storage.pop_front();
        self.is_cleared = true;
    }
}

pub struct InputSpan<'a, T> {
    storage: ReadBufferGuard<'a, ArenaPtr<Message<T>>>,
}

impl<'a, T: 'static> InputSpan<'_, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> InputSpan<'a, T> {
        InputSpan {
            storage: subscriber.buffers.get_read_buffer(),
        }
    }
    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> InputSpan<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        InputSpan::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn inputs(&mut self) -> impl Iterator<Item = &T> {
        self.storage.as_slice().iter().map(|ptr|
                // SAFETY: Publisher guarantees that the value has been initialized and
                // any value in a subscriber queue cannot be modified by a publisher.
                unsafe { &(*ptr.payload.get()).assume_init_ref().message })
    }
}
