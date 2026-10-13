use crate::message::Message;
use base::arena::ArenaReaderPtr;

/// Bind a forwarded publisher key to the lifetime of its source storage.
pub fn publisher_key<'a, M, S: ForwardedSources>(
    key: crate::PublisherKey<Forwarded<M, S::Bound<'static>>>,
) -> crate::PublisherKey<Forwarded<M, S::Bound<'a>>> {
    key.retype()
}

/// Bind a forwarded subscriber key to the lifetime of its source storage.
pub fn subscriber_key<'a, M, S: ForwardedSources>(
    key: crate::SubscriberKey<Forwarded<M, S::Bound<'static>>>,
) -> crate::SubscriberKey<Forwarded<M, S::Bound<'a>>> {
    key.retype()
}

/// An output value and named, owning references to its source publications.
/// Sources may be one arena handle (mapped as `source`) or a struct deriving
/// `ForwardedSources` with any number of named heterogeneous handles.
pub struct Forwarded<MessageType, ForwardedMessages> {
    pub message: MessageType,
    pub forwarded_messages: ForwardedMessages,
}
impl<MessageType, ForwardedMessages> Forwarded<MessageType, ForwardedMessages> {
    pub fn new(message: MessageType, forwarded_messages: ForwardedMessages) -> Self {
        Self {
            message,
            forwarded_messages,
        }
    }
}

/// Lifetime-indexed source bundle, normally implemented by the ForwardedSources derive.
///
/// # Safety
/// Rebinding must change only the lifetime of ArenaReaderPtr fields, retaining
/// identical layout and destruction behavior for every Bound lifetime.
/// This must also hold for nested source payloads and enclosing arena/channel
/// storage types: named storage reinterprets those types under the substitution.
/// Every field must be described exactly once, in declaration order.
pub unsafe trait ForwardedSources: Send + Sync + 'static {
    type Bound<'a>: Send + Sync + 'a;
    fn fields() -> Vec<(&'static str, std::any::TypeId)>;
}

pub trait SerializableForward: Sized {
    type Context<'a>
    where
        Self: 'a;
    fn headers(&self) -> Vec<(&'static str, crate::message::MessageHeader)>;

    /// Given a header, find the deserialized payload
    fn resolve<'a>(
        headers: &[(String, crate::message::MessageHeader)],
        context: Self::Context<'a>,
    ) -> Result<Self, crate::loggable::DeserializeError>
    where
        Self: 'a;
}

// SAFETY: The rebound type changes only the source handle's lifetime.
unsafe impl<S: Send + Sync + 'static> ForwardedSources for ArenaReaderPtr<'static, Message<S>> {
    type Bound<'a> = ArenaReaderPtr<'a, Message<S>>;
    fn fields() -> Vec<(&'static str, std::any::TypeId)> {
        vec![("source", std::any::TypeId::of::<S>())]
    }
}
impl<'storage, T> SerializableForward for ArenaReaderPtr<'storage, Message<T>> {
    type Context<'a>
        = &'a dyn crate::loggable::MessageLog<'storage, T>
    where
        Self: 'a;
    fn headers(&self) -> Vec<(&'static str, crate::message::MessageHeader)> {
        vec![("source", self.header)]
    }

    fn resolve<'a>(
        headers: &[(String, crate::message::MessageHeader)],
        context: Self::Context<'a>,
    ) -> Result<Self, crate::loggable::DeserializeError>
    where
        Self: 'a,
    {
        if headers.len() != 1 || headers[0].0 != "source" {
            return Err("expected one forwarded source field named 'source'".into());
        }
        context
            .lookup(&headers[0].1)
            .ok_or_else(|| "missing or ambiguous forwarded source publication".into())
    }
}

#[cfg(feature = "serde")]
impl<MessageType, SerializableType> crate::loggable::Loggable
    for Forwarded<MessageType, SerializableType>
where
    MessageType: serde::Serialize + serde::de::DeserializeOwned,
    SerializableType: SerializableForward,
{
    type Context<'a>
        = SerializableType::Context<'a>
    where
        Self: 'a;
    fn serialize(
        &self,
        writer: &mut dyn std::io::Write,
    ) -> Result<(), crate::loggable::SerializeError> {
        serde_json::to_writer(writer, &(&self.message, self.forwarded_messages.headers()))
            .map_err(Into::into)
    }
    fn deserialize_with_ctx<'a>(
        bytes: &[u8],
        context: Self::Context<'a>,
    ) -> Result<Self, crate::loggable::DeserializeError>
    where
        Self: 'a,
    {
        let (message, headers): (MessageType, Vec<(String, crate::message::MessageHeader)>) =
            serde_json::from_slice(bytes)?;
        Ok(Self::new(
            message,
            SerializableType::resolve(&headers, context)?,
        ))
    }
}
