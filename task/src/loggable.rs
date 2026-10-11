//! Typed serialization and arena-backed forwarded-message resolution.
use crate::message::{Message, MessageHeader};
use base::arena::ArenaReaderPtr;
use std::io::Write;

pub type SerializeError = Box<dyn std::error::Error + Send + Sync>;
pub type DeserializeError = Box<dyn std::error::Error + Send + Sync>;
pub trait Loggable: Sized {
    type Context<'a>
    where
        Self: 'a;
    fn serialize(&self, writer: &mut dyn Write) -> Result<(), SerializeError>;
    fn deserialize_with_ctx<'a>(
        bytes: &[u8],
        context: Self::Context<'a>,
    ) -> Result<Self, DeserializeError>
    where
        Self: 'a;
    fn deserialize(bytes: &[u8]) -> Result<Self, DeserializeError>
    where
        Self: 'static,
        for<'a> Self: Loggable<Context<'a> = ()>,
    {
        Self::deserialize_with_ctx(bytes, ())
    }
}
#[cfg(feature = "serde")]
impl<T: serde::Serialize + serde::de::DeserializeOwned> Loggable for T {
    type Context<'a>
        = ()
    where
        Self: 'a;
    fn serialize(&self, writer: &mut dyn Write) -> Result<(), SerializeError> {
        serde_json::to_writer(writer, self).map_err(Into::into)
    }
    fn deserialize_with_ctx<'a>(bytes: &[u8], _: ()) -> Result<Self, DeserializeError>
    where
        Self: 'a,
    {
        serde_json::from_slice(bytes).map_err(Into::into)
    }
}
pub trait MessageLog<'storage, T> {
    fn lookup(&self, header: &MessageHeader) -> Option<ArenaReaderPtr<'storage, Message<T>>>;
}
/// Retains source messages without requiring Clone on their payloads.
pub struct ReplayMessageLog<'storage, T> {
    messages: Vec<ArenaReaderPtr<'storage, Message<T>>>,
}
impl<'storage, T> ReplayMessageLog<'storage, T> {
    pub fn new(messages: Vec<ArenaReaderPtr<'storage, Message<T>>>) -> Self {
        Self { messages }
    }
}
impl<'storage, T> MessageLog<'storage, T> for ReplayMessageLog<'storage, T> {
    fn lookup(&self, header: &MessageHeader) -> Option<ArenaReaderPtr<'storage, Message<T>>> {
        let mut matches = self.messages.iter().filter(|m| m.header == *header);
        let found = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(found.clone())
    }
}
