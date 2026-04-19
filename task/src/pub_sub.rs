use crate::message_header::MessageHeader;

pub type ChannelName = String;
pub type CallbackName = String;

/// Message type that i should standardize on
pub struct Message<T> {
    pub header: MessageHeader,
    pub message: T,
}
