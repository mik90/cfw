use crate::{arena::ArenaPtr, pub_sub::ChannelName};

/// Special message type that includes handles to other messages
/// Allows you to re-publish messages without copying their contents
///
/// A callback can declare this as a published message and this will allow the pub/sub system
/// to allocate the correct amount of messages since forwarding messages extends the capacity of the channel in the pub/sub system.
///
/// TODO: Is this needed?
pub trait MessageWithForwards {
    /// Returns set of channels that this message may be forwarding from
    fn get_forwarded_channels(&self) -> &[&ChannelName];
}

/// A single forwarded message.
/// For multiple, i think i need multiple traits since we don't have variadics?
pub struct ForwardedMessage<T, F> {
    message: T,
    forwarded_message: ArenaPtr<F>,
}

impl<T, F> ForwardedMessage<T, F> {
    pub fn get_message(&self) -> &T {
        &self.message
    }

    pub fn get_forwarded_message(&self) -> &F {
        todo!("should return a ref to the forwarded message")
    }
}
