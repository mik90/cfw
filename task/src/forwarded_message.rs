use std::ops::Deref;

use crate::{arena::ArenaPtr, pub_sub::ChannelName};

/// Special message type that includes handles to other messages
/// Allows you to re-publish messages without copying their contents
///
/// A callback can declare this as a published message and this will allow the pub/sub system
/// to allocate the correct amount of messages since forwarding messages extends the capacity of the channel in the pub/sub system.
pub trait MessageWithForwards {
    /// Returns set of channels that this message may be forwarding from
    fn get_forwarded_channels(&self) -> &[&ChannelName];
}

pub struct ForwardedMessage<T> {
    message: ArenaPtr<T>,
}

impl<T> Deref for ForwardedMessage<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: A forwarded message must have been created from an already-initialized arena slot
        unsafe { (*self.message.payload.get()).assume_init_ref() }
    }
}
