use crate::arena::ArenaReaderPtr;
use crate::callback::CallbackReadiness;
use crate::double_buffer::{DoubleBuffer, ReadBufferGuard, WriteBufferHandle};
use crate::forwarded_message::ForwardMessageTrait;
use crate::generic_subscriber;
pub use crate::generic_subscriber::GenericSubscriber;
use crate::message::Message;
use crate::pub_sub::ChannelName;
use core::panic;
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

pub struct RequiredInput<'a, T> {
    _subscriber: &'a Subscriber<T>,
    guard: ReadBufferGuard<'a, Message<T>>,
}

impl<'a, T: 'static> RequiredInput<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> RequiredInput<'a, T> {
        let guard = subscriber.get_read_buffer();
        if guard.front().is_none() {
            panic!("RequiredInput should only have been constructed on non-empty read-buffer");
        }
        RequiredInput {
            _subscriber: subscriber,
            guard,
        }
    }
    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> RequiredInput<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        RequiredInput::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn value(&self) -> &T {
        // TODO can we just do this check once on construction and avoid this expect?
        // We could use unwrap_unchecked(), but there has to be a safe way to store both the guard and the ref together
        &self
            .guard
            .front()
            .expect("RequiredInput should only have been constructed on non-empty read-buffer")
            .message
    }
}

impl<'a, T: 'static> Deref for RequiredInput<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value()
    }
}

pub struct ForwardableRequiredInput<'a, T> {
    input: RequiredInput<'a, T>,
}

impl<'a, T: 'static + ForwardMessageTrait> ForwardableRequiredInput<'a, T> {
    pub fn new(forwardable_subscriber: &'a ForwardableSubscriber<T>) -> Self {
        let input = RequiredInput::new(&forwardable_subscriber.subscriber);
        Self { input }
    }

    /// TODO: Should we directly take in a subscriber to avoid the case where a user holds onto this?
    /// Returns a forwardable pointer that can be used in a publisher's output
    pub fn forward(mut self) -> ArenaReaderPtr<Message<T>> {
        self.input
            .guard
            .pop_front_ptr()
            .map(|ptr| ArenaReaderPtr::new(ptr))
            .expect("Expected proc macro to use the correct types")
    }

    pub fn value(&self) -> &T {
        self.input.value()
    }
}

impl<'a, T: 'static> Deref for ForwardableRequiredInput<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.input.value()
    }
}

pub struct OptionalInput<'a, T> {
    subscriber: &'a Subscriber<T>,
    guard: ReadBufferGuard<'a, Message<T>>,
}

impl<'a, T: 'static> OptionalInput<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> OptionalInput<'a, T> {
        let guard = subscriber.get_read_buffer();
        OptionalInput { subscriber, guard }
    }
    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> OptionalInput<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        OptionalInput::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn value(&'a self) -> Option<&'a T> {
        self.guard.front().map(|ptr| &ptr.message)
    }

    pub fn clear(&'a mut self) {
        self.guard.pop_front();
    }
}

pub struct ForwardableOptionalInput<'a, T> {
    input: OptionalInput<'a, T>,
}

impl<'a, T: 'static> ForwardableOptionalInput<'a, T> {
    pub fn new(forwardable_subscriber: &'a ForwardableSubscriber<T>) -> Self {
        let input = OptionalInput::new(&forwardable_subscriber.subscriber);
        Self { input }
    }

    /// TODO: Should we directly take in a subscriber to avoid the case where a user holds onto this?
    /// Returns an optional forwardable pointer that can be used in a publisher's output
    pub fn forward(mut self) -> Option<ArenaReaderPtr<Message<T>>> {
        self.input
            .guard
            .pop_front_ptr()
            .map(|ptr| ArenaReaderPtr::new(ptr))
    }

    pub fn value(&'a self) -> Option<&'a T> {
        self.input.value()
    }

    pub fn clear(&'a mut self) {
        self.input.clear();
    }
}

pub struct InputSpan<'a, T> {
    _subscriber: &'a Subscriber<T>,
    guard: ReadBufferGuard<'a, Message<T>>,
}

impl<'a, T: 'static> InputSpan<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> InputSpan<'a, T> {
        let guard = subscriber.get_read_buffer();
        InputSpan {
            _subscriber: subscriber,
            guard,
        }
    }
    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> InputSpan<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        InputSpan::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn inputs(&mut self) -> impl Iterator<Item = &Message<T>> {
        self.guard.as_slice()
    }
}

pub struct ForwardableInputSpan<'a, T> {
    input: InputSpan<'a, T>,
}

impl<'a, T: 'static + ForwardMessageTrait> ForwardableInputSpan<'a, T> {
    pub fn new(forwardable_subscriber: &'a ForwardableSubscriber<T>) -> Self {
        let input = InputSpan::new(&forwardable_subscriber.subscriber);
        Self { input }
    }

    /// TODO: Should we directly take in a subscriber to avoid the case where a user holds onto this?
    /// TODO: Can we return some Drain wrapper that manages ownership instead of letting users call drain_forwards on the InputSpan twice?
    /// Creates iterator that drains elements from storage
    pub fn drain_forwards(&mut self) -> impl Iterator<Item = ArenaReaderPtr<Message<T>>> {
        self.input.guard.drain_contiguous()
    }
}
