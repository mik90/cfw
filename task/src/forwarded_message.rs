use crate::message::Message;
use base::arena::ArenaReaderPtr;

/// An output value and named, owning references to its source publications.
/// Sources may be one arena handle (mapped as `source`) or a struct deriving
/// `ForwardedSources` with any number of named heterogeneous handles.
pub struct Forwarded<M, S> {
    pub message: M,
    pub sources: S,
}
impl<M, S> Forwarded<M, S> {
    pub fn new(message: M, sources: S) -> Self {
        Self { message, sources }
    }
}

/// Lifetime-indexed source bundle, normally implemented by the ForwardedSources derive.
///
/// # Safety
/// Rebinding must change only the lifetime of ArenaReaderPtr fields, retaining
/// identical layout and destruction behavior for every Bound lifetime.
/// Every field must be described exactly once, in declaration order.
pub unsafe trait ForwardedSources: Send + Sync + 'static {
    type Bound<'a>: Send + Sync + 'a;
    fn fields() -> Vec<(&'static str, std::any::TypeId)>;
}

pub trait SourceCodec: Sized {
    type Context<'a>
    where
        Self: 'a;
    fn headers(&self) -> Vec<(&'static str, crate::message::MessageHeader)>;
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
impl<'storage, T> SourceCodec for ArenaReaderPtr<'storage, Message<T>> {
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
impl<M, S> crate::loggable::Loggable for Forwarded<M, S>
where
    M: serde::Serialize + serde::de::DeserializeOwned,
    S: SourceCodec,
{
    type Context<'a>
        = S::Context<'a>
    where
        Self: 'a;
    fn serialize(
        &self,
        writer: &mut dyn std::io::Write,
    ) -> Result<(), crate::loggable::SerializeError> {
        serde_json::to_writer(writer, &(&self.message, self.sources.headers())).map_err(Into::into)
    }
    fn deserialize_with_ctx<'a>(
        bytes: &[u8],
        context: Self::Context<'a>,
    ) -> Result<Self, crate::loggable::DeserializeError>
    where
        Self: 'a,
    {
        let (message, headers): (M, Vec<(String, crate::message::MessageHeader)>) =
            serde_json::from_slice(bytes)?;
        Ok(Self::new(message, S::resolve(&headers, context)?))
    }
}
