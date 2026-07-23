use crate::{
    message::MessageHeader,
    pub_sub::{CallbackNodeName, ChannelName},
    time::FrameworkTime,
};

#[derive(Debug, Copy, Clone)]
pub struct PublishedMessage {
    pub publisher_index: usize,
    pub header: MessageHeader,
}

impl PublishedMessage {
    pub fn is_valid(&self) -> bool {
        self.header.published_at != FrameworkTime::INVALID
    }
}

#[derive(Debug, Copy, Clone)]
pub struct RecievedMessage {
    pub subscriber_index: usize,
    pub header: MessageHeader,
}

impl RecievedMessage {
    pub fn is_valid(&self) -> bool {
        self.header.published_at != FrameworkTime::INVALID
    }
}

/// Does not need to capture entire state, can be split across multiple messages
#[derive(Debug)]
pub struct ExecutionLogEntry {
    callback_node_index: usize,
    execution_time: FrameworkTime,
    published_messages: [PublishedMessage; 24],
    recieved_messages: [RecievedMessage; 24],
}

impl ExecutionLogEntry {
    pub fn is_valid(&self) -> bool {
        self.execution_time != FrameworkTime::INVALID
    }
}

/// Fixed size message
/// TODO: We'll need to send a mapping between index->names to be useful
#[derive(Debug)]
pub struct ExecutionLogMessage {
    number_of_dropped_entries: usize,
    entries: [ExecutionLogEntry; 256],
}

/// Descriptors sent at startup to normalize parsing

#[derive(Debug)]
pub struct SubscriberDescriptor {
    pub name: ChannelName,
    pub queue_size: usize,
    pub is_trigger: bool,
}

#[derive(Debug)]
pub struct PublisherDescriptor {
    pub name: ChannelName,
    pub queue_size: usize,
}

#[derive(Debug)]
pub struct CallbackNodeDescriptor {
    pub name: CallbackNodeName,
    pub subscribers: Vec<SubscriberDescriptor>,
    pub publishers: Vec<PublisherDescriptor>,
}

///Sent at startup
#[derive(Debug)]
pub struct ExecutionLogSchema {
    callbacks: Vec<CallbackNodeDescriptor>,
}
