use crate::generic_subscriber::GenericSubscriber;
use crate::message::Message;
use crate::output::{ForwardedOutput, ForwardedOutputSpan, ForwardingOutput};
use crate::subscriber::{ForwardableSubscriber, Subscriber};
use base::arena::ArenaReaderPtr;
use base::double_buffer::{DoubleBuffer, ReadBufferGuard};
use std::ops::Deref;

pub struct RequiredInput<'a, T> {
    buffers: &'a DoubleBuffer<Message<T>>,
    guard: ReadBufferGuard<'a, Message<T>>,
}

impl<'a, T: 'static> RequiredInput<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> RequiredInput<'a, T> {
        let buffers = subscriber.buffers();
        let guard = buffers.read_buffer();
        if guard.front().is_none() {
            panic!("RequiredInput should only have been constructed on non-empty read-buffer");
        }
        RequiredInput { buffers, guard }
    }

    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> RequiredInput<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        RequiredInput::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn value(&self) -> &T {
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

impl<'a, T: 'static> ForwardableRequiredInput<'a, T> {
    pub fn new(forwardable_subscriber: &'a ForwardableSubscriber<T>) -> Self {
        let input = RequiredInput::new(&forwardable_subscriber.subscriber);
        Self { input }
    }

    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> Self {
        let typed = subscriber
            .as_any()
            .downcast_mut::<ForwardableSubscriber<T>>()
            .expect("Expected proc macro to use the correct types");
        ForwardableRequiredInput::new(typed)
    }

    pub fn value(&self) -> &T {
        self.input.value()
    }

    pub fn forward<'b, UserData: Default + 'static>(
        mut self,
        output: &'b mut ForwardingOutput<UserData, T>,
    ) -> ForwardedOutput<'b, UserData, T> {
        let ptr = self
            .input
            .guard
            .pop_front_ptr()
            .map(ArenaReaderPtr::new)
            .expect("Expected proc macro to use the correct types");
        ForwardedOutput::new(output.publisher, ptr)
    }
}

impl<'a, T: 'static> Deref for ForwardableRequiredInput<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.input.value()
    }
}

pub struct OptionalInput<'a, T> {
    buffers: &'a DoubleBuffer<Message<T>>,
    guard: ReadBufferGuard<'a, Message<T>>,
}

impl<'a, T: 'static> OptionalInput<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> OptionalInput<'a, T> {
        let buffers = subscriber.buffers();
        let guard = buffers.read_buffer();
        OptionalInput { buffers, guard }
    }

    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> OptionalInput<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        OptionalInput::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn value(&self) -> Option<&T> {
        self.guard.front().map(|ptr| &ptr.message)
    }

    pub fn clear(&mut self) {
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

    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> Self {
        let typed = subscriber
            .as_any()
            .downcast_mut::<ForwardableSubscriber<T>>()
            .expect("Expected proc macro to use the correct types");
        ForwardableOptionalInput::new(typed)
    }

    pub fn forward<'b, UserData: Default + 'static>(
        mut self,
        output: &'b mut ForwardingOutput<UserData, T>,
    ) -> Option<ForwardedOutput<'b, UserData, T>> {
        let ptr = self.input.guard.pop_front_ptr().map(ArenaReaderPtr::new)?;
        Some(ForwardedOutput::new(output.publisher, ptr))
    }

    pub fn value(&self) -> Option<&T> {
        self.input.value()
    }

    pub fn clear(&mut self) {
        self.input.clear();
    }
}

pub struct InputSpan<'a, T> {
    buffers: &'a DoubleBuffer<Message<T>>,
    guard: ReadBufferGuard<'a, Message<T>>,
}

impl<'a, T: 'static> InputSpan<'a, T> {
    pub fn new(subscriber: &'a Subscriber<T>) -> InputSpan<'a, T> {
        let buffers = subscriber.buffers();
        let guard = buffers.read_buffer();
        InputSpan { buffers, guard }
    }

    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> InputSpan<'a, T> {
        let typed_subscriber = subscriber.as_any().downcast_mut::<Subscriber<T>>();
        InputSpan::new(typed_subscriber.expect("Expected proc macro to use the correct types"))
    }

    pub fn inputs(&mut self) -> impl Iterator<Item = &Message<T>> {
        self.guard.as_slice()
    }

    /// Drains the read buffer, yielding `ArenaReaderPtr<Message<T>>` for each
    /// queued message. `ArenaReaderPtr<T>` derefs to `T` transparently, so
    /// callers can write `for msg in span.drain_inputs() { let msg: &Message<T> = &msg; ... }`
    /// without dealing with the pointer type explicitly. Draining removes
    /// messages from the queue so later runs see a fresh buffer — used by the
    /// `LogTask`'s type-erased serializer closures, which need to consume
    /// inputs each cycle.
    pub fn drain_inputs(&mut self) -> impl Iterator<Item = ArenaReaderPtr<Message<T>>> {
        self.guard.drain_contiguous()
    }
}

pub struct ForwardableInputSpan<'a, T> {
    input: InputSpan<'a, T>,
}

impl<'a, T: 'static> ForwardableInputSpan<'a, T> {
    pub fn new(forwardable_subscriber: &'a ForwardableSubscriber<T>) -> Self {
        let input = InputSpan::new(&forwardable_subscriber.subscriber);
        Self { input }
    }

    pub fn new_downcasted(subscriber: &'a mut dyn GenericSubscriber) -> Self {
        let typed = subscriber
            .as_any()
            .downcast_mut::<ForwardableSubscriber<T>>()
            .expect("Expected proc macro to use the correct types");
        ForwardableInputSpan::new(typed)
    }

    pub fn drain_forwards<'b, UserData: Default + 'static>(
        &mut self,
        output: &'b mut ForwardingOutput<UserData, T>,
    ) -> ForwardedOutputSpan<'b, UserData, T> {
        ForwardedOutputSpan::new(output.publisher, self.input.guard.drain_contiguous())
    }
}
