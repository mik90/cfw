use crate::Subscriber;
pub use crate::subscriber::Input;
use std::ops::Deref;

/// Default callback input capability. Consumed handles cannot escape the input borrow.
/// ```compile_fail
/// use task::{Input, ArenaReaderPtr, message::Message};
/// fn extend<'input, 'storage>(mut input: Input<'input,'storage,u64>) -> ArenaReaderPtr<'storage,Message<u64>> {
///     input.pop().unwrap()
/// }
/// ```
pub struct ReadOnly;
/// Allows source handles to be transferred into storage-lived output loans.
pub struct Forwardable;
pub type OptionalInput<'input, 'storage, T, C = ReadOnly> = Input<'input, 'storage, T, C>;
pub type InputSpan<'input, 'storage, T, C = ReadOnly> = Input<'input, 'storage, T, C>;

/// A read view constructed after the executor checks required-input readiness.
pub struct RequiredInput<'input, 'storage, T, C = ReadOnly>(Input<'input, 'storage, T, C>);

impl<'input, 'storage, T> RequiredInput<'input, 'storage, T> {
    pub fn new(subscriber: &'input Subscriber<'storage, T>) -> Self {
        assert!(!subscriber.is_empty(), "required input is empty");
        Self(subscriber.read_input())
    }
}
impl<'input, 'storage, T> RequiredInput<'input, 'storage, T, Forwardable> {
    pub fn new_forwardable(subscriber: &'input Subscriber<'storage, T>) -> Self {
        assert!(!subscriber.is_empty(), "required input is empty");
        Self(subscriber.forwardable_input())
    }
    pub fn forward(
        &mut self,
    ) -> Option<base::arena::ArenaReaderPtr<'storage, crate::message::Message<T>>> {
        self.0.forward()
    }
}
impl<T, C> RequiredInput<'_, '_, T, C> {
    pub fn value(&self) -> &T {
        self.0.value().expect("required input is empty")
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

impl<T, C> Deref for RequiredInput<'_, '_, T, C> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value()
    }
}
