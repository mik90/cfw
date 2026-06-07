use crate::time::FrameworkTime;

/// Metadata attached to every message as it passes through the pub/sub system.
/// Set by the executor at flush time — the executor is the sole source of time.
#[derive(Clone)]
pub struct MessageHeader {
    pub published_at: FrameworkTime,
}

impl Default for MessageHeader {
    fn default() -> Self {
        MessageHeader {
            published_at: FrameworkTime::INVALID,
        }
    }
}

/// Contiguous message struct for payload and header.
/// Meant for allocation in arenas
pub struct Message<T> {
    pub header: MessageHeader,
    pub message: T,
}

impl<T: Clone> Clone for Message<T> {
    fn clone(&self) -> Self {
        Self {
            header: self.header.clone(),
            message: self.message.clone(),
        }
    }
}

/// Default constructible T means Message is default constructible
impl<T: Default> Default for Message<T> {
    fn default() -> Self {
        Self {
            header: MessageHeader::default(),
            message: T::default(),
        }
    }
}
