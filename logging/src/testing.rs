use crate::{BoxedLogError, LogFileWriter};
use std::sync::{Arc, Mutex};
use task::message::MessageHeader;
use task::pub_sub::ChannelName;

#[derive(Default, Debug, Clone)]
pub struct InMemoryMessage {
    channel: ChannelName,
    header: MessageHeader,
    body: Vec<u8>,
}

#[derive(Default, Debug, Clone)]
pub struct InMemoryArtifact {
    name: String,
    body: Vec<u8>,
}

#[derive(Default, Debug, Clone)]
pub struct LoggedData {
    messages: Vec<InMemoryMessage>,
    artifacts: Vec<InMemoryArtifact>,
}

#[derive(Debug)]
pub struct InMemoryWriter {
    data: Arc<Mutex<LoggedData>>,
}

impl InMemoryWriter {
    pub fn new() -> Self {
        InMemoryWriter {
            data: Arc::new(Mutex::new(LoggedData::default())),
        }
    }

    pub fn logged_data(&self) -> Arc<Mutex<LoggedData>> {
        self.data.clone()
    }
}

impl LogFileWriter for InMemoryWriter {
    fn store_message(
        &mut self,
        channel_name: &str,
        header: &MessageHeader,
        body: &[u8],
    ) -> Result<(), BoxedLogError> {
        self.data
            .lock()
            .expect("Poisoned logged data")
            .messages
            .push(InMemoryMessage {
                channel: channel_name.into(),
                header: header.clone(),
                body: body.into(),
            });
        Ok(())
    }

    fn write_artifact(&mut self, name: &str, body: &[u8]) -> Result<(), BoxedLogError> {
        self.data
            .lock()
            .expect("Poisoned logged data")
            .artifacts
            .push(InMemoryArtifact {
                name: name.into(),
                body: body.into(),
            });
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BoxedLogError> {
        Ok(())
    }
}
