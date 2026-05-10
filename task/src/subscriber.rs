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

#[allow(dead_code)]
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

    pub fn get_read_buffer<'a>(&'a self) -> ReadBufferGuard<'a, ArenaPtr<Message<T>>> {
        self.buffers.get_read_buffer()
    }

    /// Clear all buffered values. Should be called before the Arena is dropped
    /// to prevent ArenaPtrs from outliving their Arena.
    pub fn cleanup_buffers(&self) {
        self.buffers.clear();
    }
}

impl<T: 'static> Subscriber<T> {
    pub fn required_input(&self) -> RequiredInput<'_, T> {
        RequiredInput::new(self)
    }

    pub fn optional_input(&self) -> OptionalInput<'_, T> {
        OptionalInput::new(self)
    }

    pub fn input_span(&self) -> InputSpan<'_, T> {
        InputSpan::new(self)
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
    _subscriber: &'a Subscriber<T>,
    ptr: &'a ArenaPtr<Message<T>>,
}

impl<'a, T: 'static> RequiredInput<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> RequiredInput<'a, T> {
        let guard = subscriber.get_read_buffer();
        // SAFETY: The read buffer VecDeque is not modified during task execution.
        // &'a Subscriber<T> ensures the subscriber (and its VecDeque allocation) lives for 'a.
        let ptr = unsafe {
            &*(guard
                .front()
                .expect("Required input storage should have been validated before construction")
                as *const ArenaPtr<Message<T>>)
        };
        RequiredInput {
            _subscriber: subscriber,
            ptr,
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
        unsafe { &(*self.ptr.payload.get()).assume_init_ref().message }
    }
}

pub struct OptionalInput<'a, T> {
    subscriber: &'a Subscriber<T>,
    ptr: Option<&'a ArenaPtr<Message<T>>>,
}

impl<'a, T: 'static> OptionalInput<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> OptionalInput<'a, T> {
        let guard = subscriber.get_read_buffer();
        // SAFETY: The read buffer VecDeque is not modified during task execution.
        // &'a Subscriber<T> ensures the subscriber (and its VecDeque allocation) lives for 'a.
        let ptr = unsafe { guard.front().map(|p| &*(p as *const ArenaPtr<Message<T>>)) };
        OptionalInput { subscriber, ptr }
    }
    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> OptionalInput<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        OptionalInput::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn value(&'a self) -> Option<&'a T> {
        self.ptr.map(|ptr|
            // SAFETY: Publisher guarantees that the value has been initialized and
            // any value in a subscriber queue cannot be modified by a publisher.
            unsafe { &(*ptr.payload.get()).assume_init_ref().message })
    }

    pub fn clear(&'a mut self) {
        if self.ptr.take().is_some() {
            self.subscriber.get_read_buffer().pop_front();
        }
    }
}

pub struct InputSpan<'a, T> {
    _subscriber: &'a Subscriber<T>,
    ptrs: &'a [ArenaPtr<Message<T>>],
}

impl<'a, T: 'static> InputSpan<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> InputSpan<'a, T> {
        let mut guard = subscriber.get_read_buffer();
        // SAFETY: The read buffer VecDeque is not modified during task execution.
        // &'a Subscriber<T> ensures the subscriber (and its VecDeque allocation) lives for 'a.
        let ptrs = unsafe { &*(guard.as_slice() as *const [ArenaPtr<Message<T>>]) };
        InputSpan {
            _subscriber: subscriber,
            ptrs,
        }
    }
    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> InputSpan<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        InputSpan::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn inputs(&self) -> impl Iterator<Item = &T> {
        self.ptrs.iter().map(|ptr|
            // SAFETY: Publisher guarantees that the value has been initialized and
            // any value in a subscriber queue cannot be modified by a publisher.
            unsafe { &(*ptr.payload.get()).assume_init_ref().message })
    }
}
