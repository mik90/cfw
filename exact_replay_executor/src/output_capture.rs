//! Exact-replay output capture implementing publisher observation.
use logging::BoxedLogError;
use std::sync::{Arc, Mutex};
use task::{
    Publisher,
    loggable::{Loggable, SerializeError},
    message::MessageHeader,
};
#[cfg(feature = "iceoryx2")]
use task_graph::capture::output::observe_ipc;
use task_graph::capture::{OutputSink, output::observe_native};

struct State {
    messages: Vec<(MessageHeader, Vec<u8>)>,
    error: Option<String>,
    capacity: usize,
}

/// Buffers serialized outputs observed at one publisher endpoint.
pub struct OutputCapture {
    channel: String,
    payload_type: String,
    state: Arc<Mutex<State>>,
}

impl OutputCapture {
    pub(crate) fn new(channel: String, payload_type: String, capacity: usize) -> Self {
        assert!(capacity > 0, "capture capacity must be positive");
        Self {
            channel,
            payload_type,
            state: Arc::new(Mutex::new(State {
                messages: Vec::new(),
                error: None,
                capacity,
            })),
        }
    }

    pub fn native<'a, T: Loggable + Send + Sync + 'a>(
        publisher: &mut Publisher<'a, T>,
        capacity: usize,
    ) -> Self {
        let capture = Self::new(
            publisher.channel_name().to_owned(),
            std::any::type_name::<T>().to_owned(),
            capacity,
        );
        observe_native(publisher, capture.sink());
        capture
    }

    #[cfg(feature = "iceoryx2")]
    pub fn ipc<T>(publisher: &mut task::iox2::Iox2Publisher<T>, capacity: usize) -> Self
    where
        T: Loggable + std::fmt::Debug + iceoryx2::prelude::ZeroCopySend + Send + Sync + 'static,
    {
        let capture = Self::new(
            publisher.channel_name().to_owned(),
            std::any::type_name::<T>().to_owned(),
            capacity,
        );
        observe_ipc(publisher, capture.sink());
        capture
    }

    pub(crate) fn sink(&self) -> Box<dyn OutputSink> {
        Box::new(CaptureSink {
            state: self.state.clone(),
        })
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn payload_type(&self) -> &str {
        &self.payload_type
    }

    pub fn clear(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.messages.clear();
        state.error = None;
    }

    pub fn take(&mut self) -> Result<Vec<(MessageHeader, Vec<u8>)>, BoxedLogError> {
        let mut state = self.state.lock().unwrap();
        let messages = std::mem::take(&mut state.messages);
        match state.error.take() {
            Some(error) => Err(error.into()),
            None => Ok(messages),
        }
    }
}

struct CaptureSink {
    state: Arc<Mutex<State>>,
}

impl OutputSink for CaptureSink {
    fn captured(&mut self, header: MessageHeader, payload: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        if state.messages.len() >= state.capacity {
            state
                .error
                .get_or_insert_with(|| "publisher capture overflow".into());
        } else {
            state.messages.push((header, payload));
        }
    }

    fn failed(&mut self, error: SerializeError) {
        let mut state = self.state.lock().unwrap();
        state.error.get_or_insert_with(|| error.to_string());
    }
}
