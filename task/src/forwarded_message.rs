use crate::message::Message;
use base::arena::ArenaReaderPtr;

/// Keeps a source message alive for the common storage borrow.
pub struct ForwardedMessage<'storage, T, F> {
    pub message: T,
    pub forwarded: ArenaReaderPtr<'storage, Message<F>>,
}

impl<'storage, T, F> ForwardedMessage<'storage, T, F> {
    pub fn new(message: T, forwarded: ArenaReaderPtr<'storage, Message<F>>) -> Self {
        Self { message, forwarded }
    }
}
