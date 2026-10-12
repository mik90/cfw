pub use crate::subscriber::Input;
use crate::{Subscriber, message::MessageHeader};
use std::ops::Deref;

/// Default callback input capability. Readers can only read the input and can't extend its lifetime.
/// ```compile_fail
/// use task::{Input, ArenaReaderPtr, message::Message};
/// fn extend<'input, 'storage>(mut input: Input<'input,'storage,u64>) -> ArenaReaderPtr<'storage,Message<u64>> {
///     input.pop().unwrap()
/// }
/// ```
pub struct ReadOnly;
/// Allows source handles to be transferred into storage-lived output loans.
pub struct Forwardable;
pub type OptionalInput<'input, 'storage, MessageType, Modifiers = ReadOnly> =
    Input<'input, 'storage, MessageType, Modifiers>;
pub type InputSpan<'input, 'storage, MessageType, Modifiers = ReadOnly> =
    Input<'input, 'storage, MessageType, Modifiers>;

/// A read view constructed after the executor checks required-input readiness.
pub struct RequiredInput<'input, 'storage, MessageType, Modifiers = ReadOnly>(
    Input<'input, 'storage, MessageType, Modifiers>,
);

impl<'input, 'storage, MessageType> RequiredInput<'input, 'storage, MessageType> {
    pub fn new(subscriber: &'input Subscriber<'storage, MessageType>) -> Self {
        // Readiness handling should've prevented this
        assert!(!subscriber.is_empty(), "required input is empty");
        Self(subscriber.input_readonly())
    }
}

impl<'input, 'storage, MessageType> RequiredInput<'input, 'storage, MessageType, Forwardable> {
    pub fn new_forwardable(subscriber: &'input Subscriber<'storage, MessageType>) -> Self {
        // Readiness handling should've prevented this
        assert!(!subscriber.is_empty(), "required input is empty");
        Self(subscriber.input_forwardable())
    }
    pub fn forward(
        &mut self,
    ) -> Option<base::arena::ArenaReaderPtr<'storage, crate::message::Message<MessageType>>> {
        self.0.forward()
    }
}
impl<MessageType, Modifiers> RequiredInput<'_, '_, MessageType, Modifiers> {
    pub fn value(&self) -> &MessageType {
        self.0.value().expect("required input is empty")
    }

    pub fn header(&self) -> &MessageHeader {
        self.0.header().expect("required input is empty")
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

impl<MessageType, Modifiers> Deref for RequiredInput<'_, '_, MessageType, Modifiers> {
    type Target = MessageType;
    fn deref(&self) -> &MessageType {
        self.value()
    }
}
