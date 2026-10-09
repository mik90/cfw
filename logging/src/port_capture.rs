//! Publisher-local serialized capture for exact output attribution.
use crate::BoxedLogError;
use std::sync::{Arc, Mutex};
use task::{
    Publisher,
    loggable::Loggable,
    message::{Message, MessageHeader},
};

struct State {
    messages: Vec<(MessageHeader, Vec<u8>)>,
    error: Option<String>,
    capacity: usize,
}
pub struct PortCapture {
    channel: String,
    payload_type: &'static str,
    state: Arc<Mutex<State>>,
}
impl PortCapture {
    fn create<T>(channel: &str, capacity: usize) -> Self {
        assert!(capacity > 0, "capture capacity must be positive");
        Self {
            channel: channel.into(),
            payload_type: std::any::type_name::<T>(),
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
        let capture = Self::create::<T>(publisher.channel_name(), capacity);
        let state = capture.state.clone();
        publisher.observe(move |message| observe(&state, message));
        capture
    }
    #[cfg(feature = "iceoryx2")]
    pub fn ipc<T>(publisher: &mut task::iox2::Iox2Publisher<T>, capacity: usize) -> Self
    where
        T: Loggable + std::fmt::Debug + iceoryx2::prelude::ZeroCopySend + Send + Sync + 'static,
    {
        let capture = Self::create::<T>(publisher.channel_name(), capacity);
        let state = capture.state.clone();
        publisher.observe(move |message| observe(&state, message));
        capture
    }
    pub fn channel(&self) -> &str {
        &self.channel
    }
    pub fn payload_type(&self) -> &str {
        self.payload_type
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
fn observe<T: Loggable>(state: &Arc<Mutex<State>>, message: &Message<T>) {
    let mut bytes = Vec::new();
    let encoded = message.message.serialize(&mut bytes);
    let mut state = state.lock().unwrap();
    if let Err(error) = encoded {
        state.error.get_or_insert_with(|| error.to_string());
    } else if state.messages.len() >= state.capacity {
        state
            .error
            .get_or_insert_with(|| "publisher capture overflow".into());
    } else {
        state.messages.push((message.header, bytes));
    }
}
