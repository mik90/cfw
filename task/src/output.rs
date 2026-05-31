use crate::arena::ArenaReaderPtr;
use crate::forwarded_message::ForwardedMessage;
use crate::generic_publisher::GenericPublisher;
use crate::message::Message;
use crate::publisher::{ForwardingPublisher, Publisher, SendError};
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

#[allow(dead_code)]
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
    pub(crate) publisher: &'a mut Publisher<T>,
    pub(crate) loaned_value_idx: usize,
    pub on_publish_failure: PublishFailureCallback,
}

impl<'a, T> Output<'a, T> {
    pub fn value(&self) -> &T {
        &self
            .publisher
            .loaned_value_at(self.loaned_value_idx)
            .value()
            .message
    }

    pub fn value_mut(&mut self) -> &mut T {
        &mut self
            .publisher
            .loaned_value_at_mut(self.loaned_value_idx)
            .value_mut()
            .message
    }
}

impl<'a, T: Default + 'static> Output<'a, T> {
    pub fn new_default(publisher: &'a mut Publisher<T>) -> Self {
        let loaned_value_idx = publisher
            .loan_default()
            .expect("We expect loans to always be available");
        Output {
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

    pub(crate) fn new_with_factory(
        publisher: &'a mut Publisher<T>,
        factory: impl FnOnce(&mut MaybeUninit<T>),
    ) -> Self {
        let loaned_value_idx = publisher
            .loan_with(factory)
            .expect("We expect loans to always be available");
        Output {
            publisher,
            loaned_value_idx,
            on_publish_failure: PublishFailureCallback::panic(),
        }
    }
}

impl<T: 'static> Deref for Output<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value()
    }
}

impl<T: 'static> DerefMut for Output<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value_mut()
    }
}

pub struct OutputSpan<'a, T> {
    loaned_value_idx_start: usize,
    loaned_value_idx_end: usize,
    publisher: &'a mut Publisher<T>,
}

impl<'a, T> OutputSpan<'a, T> {
    pub fn outputs(&self) -> impl Iterator<Item = &T> {
        self.publisher
            .loaned_values_at(self.loaned_value_idx_start, self.loaned_value_idx_end)
            .iter()
            .map(|loaned_value|
                // SAFETY: Publisher guarantees the value has been initialized on loan
                // and a loaned value is exclusive access.
                unsafe { &(*loaned_value.ptr.payload.get()).assume_init_ref().message })
    }

    pub fn outputs_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.publisher
            .loaned_values_at_mut(self.loaned_value_idx_start, self.loaned_value_idx_end)
            .iter_mut()
            .map(|loaned_value|
                // SAFETY: Publisher guarantees the value has been initialized on loan
                // and a loaned value is exclusive access.
                unsafe { &mut (*loaned_value.ptr.payload.get()).assume_init_mut().message })
    }

    pub(crate) fn new_with_factory(
        publisher: &'a mut Publisher<T>,
        mut factory: impl FnMut(&mut MaybeUninit<T>),
    ) -> Self {
        let count = publisher.get_config().capacity;
        let start = publisher.loaned_count();
        for _ in 0..count {
            publisher.loan_with(|slot| factory(slot)).unwrap();
        }
        OutputSpan {
            loaned_value_idx_start: start,
            loaned_value_idx_end: start + count - 1,
            publisher,
        }
    }
}

impl<'a, T: Default + 'static> OutputSpan<'a, T> {
    pub fn new(publisher: &'a mut Publisher<T>) -> Self {
        for _ in 0..publisher.get_config().capacity {
            publisher.loan_default().unwrap();
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
}

pub struct ForwardingOutput<'a, T, F> {
    inner: Output<'a, ForwardedMessage<T, F>>,
}

impl<'a, T: 'static, F: 'static> ForwardingOutput<'a, T, F> {
    pub fn value(&self) -> &T {
        &self.inner.value().message
    }

    pub fn value_mut(&mut self) -> &mut T {
        &mut self.inner.value_mut().message
    }
}

impl<'a, T: Default + 'static, F: 'static> ForwardingOutput<'a, T, F> {
    pub(crate) fn new(
        publisher: &'a mut ForwardingPublisher<T, F>,
        forwarded_ptr: ArenaReaderPtr<Message<F>>,
    ) -> Self {
        let loaned_value_idx = publisher
            .inner
            .loan_forwarded(forwarded_ptr)
            .expect("We expect loans to always be available");

        let output = Output {
            loaned_value_idx,
            publisher: &mut publisher.inner,
            on_publish_failure: PublishFailureCallback::panic(),
        };
        ForwardingOutput { inner: output }
    }

    pub fn send(self) {
        self.inner.send();
    }
}

impl<T: 'static, F: 'static> Deref for ForwardingOutput<'_, T, F> {
    type Target = T;

    fn deref(&self) -> &T {
        self.value()
    }
}

impl<T: 'static, F: 'static> DerefMut for ForwardingOutput<'_, T, F> {
    fn deref_mut(&mut self) -> &mut T {
        self.value_mut()
    }
}

pub struct ForwardingOutputSpan<'a, T, F> {
    inner: OutputSpan<'a, ForwardedMessage<T, F>>,
}

impl<'a, T: Default + 'static, F: 'static> ForwardingOutputSpan<'a, T, F> {
    pub(crate) fn new(
        publisher: &'a mut ForwardingPublisher<T, F>,
        forwarded_ptrs: impl IntoIterator<Item = ArenaReaderPtr<Message<F>>>,
    ) -> Self {
        let mut ptrs = forwarded_ptrs.into_iter();
        ForwardingOutputSpan {
            inner: OutputSpan::new_with_factory(&mut publisher.inner, |slot| {
                let ptr = ptrs
                    .next()
                    .expect("not enough forwarded ptrs for span capacity");
                slot.write(ForwardedMessage::new_with_forward(ptr));
            }),
        }
    }

    pub fn outputs(&self) -> impl Iterator<Item = &T> {
        self.inner.outputs().map(|fwd| &fwd.message)
    }

    pub fn outputs_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.inner.outputs_mut().map(|fwd| &mut fwd.message)
    }
}
