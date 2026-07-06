// The log task should be able to take in arbitrary channels, handle serialization, and write to disk.
//
// The complicated part will be the introspection on the pub_sub system where we need to create subscribers for this task based on
// all the tasks that exist in the graph.
//
// So maybe this'll be a connection step after all user tasks are connected. Some task builder could order a bunch of steps in case we have
// other infrastructure-y steps.

use std::{collections::HashMap, path::PathBuf};
use task::{
    callback::Callback,
    loggable::{Loggable, SerializeError},
    message::Message,
    pub_sub::ChannelName,
};

pub(crate) struct ChannelLogger {}

impl ChannelLogger {
    pub fn serialize_message<T>(
        &self,
        message: &Message<T>,
        buffer: &mut Vec<u8>,
    ) -> Result<(), SerializeError>
    where
        Message<T>: Loggable,
    {
        message.serialize(buffer)?;
        Ok(())
    }
}

pub struct LogTaskConfiguration {
    output_path: PathBuf,
    /// Logged channel to queue capacity
    channel_to_queue_capacity: HashMap<ChannelName, usize>,
}

pub struct LogTask {
    config: LogTaskConfiguration,
}

impl LogTask {
    pub fn new(config: LogTaskConfiguration) -> Self {
        Self { config }
    }
}

impl Callback for LogTask {
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn task::subscriber::GenericSubscriber>],
        publishers: &mut [Box<dyn task::publisher::GenericPublisher>],
        ctx: &task::context::Context,
    ) -> task::callback::Run {
        task::callback::Run::new(1)
    }

    fn build_publishers(&self) -> Vec<Box<dyn task::publisher::GenericPublisher>> {
        // We could publish some diagnostics or something here
        vec![]
    }

    fn build_subscribers(&self) -> Vec<Box<dyn task::subscriber::GenericSubscriber>> {
        // TODO we should create these subscriber in the build step and return them here
        vec![]
    }
}
