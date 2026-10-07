use crate::Subscriber;
pub use crate::subscriber::Input;
use std::ops::Deref;

pub type OptionalInput<'input, 'storage, T> = Input<'input, 'storage, T>;
pub type InputSpan<'input, 'storage, T> = Input<'input, 'storage, T>;

/// A read view constructed after the executor checks required-input readiness.
pub struct RequiredInput<'input, 'storage, T>(Input<'input, 'storage, T>);

impl<'input, 'storage, T> RequiredInput<'input, 'storage, T> {
    pub fn new(subscriber: &'input Subscriber<'storage, T>) -> Self {
        assert!(!subscriber.is_empty(), "required input is empty");
        Self(subscriber.input())
    }

    pub fn value(&self) -> &T {
        self.0.value().expect("required input is empty")
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

impl<T> Deref for RequiredInput<'_, '_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value()
    }
}
