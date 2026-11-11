//! Typed serialization and arena-backed forwarded-message resolution.
use crate::message::{Message, MessageHeader};
use base::arena::ArenaReaderPtr;
use std::{io::Write, mem::MaybeUninit};

pub type SerializeError = Box<dyn std::error::Error + Send + Sync>;
pub type DeserializeError = Box<dyn std::error::Error + Send + Sync>;
pub trait Loggable: Sized {
    type Context<'a>
    where
        Self: 'a;
    fn serialize(&self, writer: &mut dyn Write) -> Result<(), SerializeError>;
    /// Decode a complete value. Types that only support in-place hydration
    /// should return an unsupported-operation error here and implement
    /// [`Self::deserialize_into`] instead.
    fn deserialize_with_ctx<'a>(
        bytes: &[u8],
        context: Self::Context<'a>,
    ) -> Result<Self, DeserializeError>
    where
        Self: 'a;
    /// Initialize `slot` in place. Types with large inline payloads can
    /// override this to avoid materializing a stack temporary.
    fn deserialize_into<'a>(
        bytes: &[u8],
        slot: &mut MaybeUninit<Self>,
        context: Self::Context<'a>,
    ) -> Result<(), DeserializeError>
    where
        Self: 'a,
    {
        slot.write(Self::deserialize_with_ctx(bytes, context)?);
        Ok(())
    }
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
pub trait MessageLog<'graph_storage, T> {
    fn lookup(&self, header: &MessageHeader) -> Option<ArenaReaderPtr<'graph_storage, Message<T>>>;
}
/// Retains source messages without requiring Clone on their payloads.
pub struct ReplayMessageLog<'graph_storage, T> {
    messages: Vec<ArenaReaderPtr<'graph_storage, Message<T>>>,
}
impl<'graph_storage, T> ReplayMessageLog<'graph_storage, T> {
    pub fn new(messages: Vec<ArenaReaderPtr<'graph_storage, Message<T>>>) -> Self {
        Self { messages }
    }
}
impl<'graph_storage, T> MessageLog<'graph_storage, T> for ReplayMessageLog<'graph_storage, T> {
    fn lookup(&self, header: &MessageHeader) -> Option<ArenaReaderPtr<'graph_storage, Message<T>>> {
        let mut matches = self.messages.iter().filter(|m| m.header == *header);
        let found = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(found.clone())
    }
}
