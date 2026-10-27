//! Publisher-local output observation for planned bindings.
use crate::{BuildError, GraphBindings};
use task::{
    Publisher,
    loggable::{Loggable, SerializeError},
    message::{Message, MessageHeader},
};

/// Receives serialized outputs observed at a publisher before fan-out.
pub trait OutputSink: Send + 'static {
    fn captured(&mut self, header: MessageHeader, payload: Vec<u8>);
    fn failed(&mut self, error: SerializeError);
}

type AttachFn =
    dyn for<'a> FnOnce(&GraphBindings<'a>, Box<dyn OutputSink>) -> Result<(), BuildError>;

/// Deferred attachment of an output sink to a planned publisher.
#[must_use = "an output binding does nothing until it is attached"]
pub struct OutputBinding(Box<AttachFn>);

impl OutputBinding {
    pub(crate) fn new<F>(attach: F) -> Self
    where
        F: for<'a> FnOnce(&GraphBindings<'a>, Box<dyn OutputSink>) -> Result<(), BuildError>
            + 'static,
    {
        Self(Box::new(attach))
    }

    /// Install the sink on the planned publisher.
    pub fn attach(
        self,
        bindings: &GraphBindings<'_>,
        sink: Box<dyn OutputSink>,
    ) -> Result<(), BuildError> {
        (self.0)(bindings, sink)
    }
}

/// Observe one native publisher, serializing each output into `sink`.
pub fn observe_native<'a, T: Loggable + Send + Sync + 'a>(
    publisher: &mut Publisher<'a, T>,
    mut sink: Box<dyn OutputSink>,
) {
    publisher.observe(move |message| forward(sink.as_mut(), message));
}

/// Observe one IPC publisher, serializing each output into `sink`.
#[cfg(feature = "iceoryx2")]
pub fn observe_ipc<T>(publisher: &mut task::iox2::Iox2Publisher<T>, mut sink: Box<dyn OutputSink>)
where
    T: Loggable + std::fmt::Debug + iceoryx2::prelude::ZeroCopySend + Send + Sync + 'static,
{
    publisher.observe(move |message| forward(sink.as_mut(), message));
}

fn forward<T: Loggable>(sink: &mut dyn OutputSink, message: &Message<T>) {
    let mut payload = Vec::new();
    match message.message.serialize(&mut payload) {
        Ok(()) => sink.captured(message.header, payload),
        Err(error) => sink.failed(error),
    }
}
