use std::any::TypeId;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::generic_subscriber::GenericSubscriber;
use crate::input::InputSpan;
use crate::loggable::{Loggable, SerializeError};
use crate::message::MessageHeader;

/// Per-message sink error — either the value failed to serialize, or the
/// writer rejected the bytes. Both surface uniformly inside the serializer
/// closure so caller-side error handling is uniform.
pub enum MessageSinkError {
    Serialize(SerializeError),
    Sink(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for MessageSinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageSinkError::Serialize(e) => write!(f, "serialize: {e}"),
            MessageSinkError::Sink(e) => write!(f, "writer: {e}"),
        }
    }
}

impl std::fmt::Debug for MessageSinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageSinkError::Serialize(e) => f.debug_tuple("Serialize").field(e).finish(),
            MessageSinkError::Sink(e) => f.debug_tuple("Sink").field(e).finish(),
        }
    }
}

impl std::error::Error for MessageSinkError {}

/// Alias for any boxed error that can be sent across threads and shared
/// between them. Used as the sink failure type so `SerializerFn` is decoupled
/// from the `logging` crate's concrete `BoxedLogError` while remaining
/// interoperable with it.
pub type BoxedError = Box<dyn std::error::Error + Send + Sync>;

/// Per-message fallible sink: writes `(header, body)` to the backing store.
/// Used as the third argument to `SerializerFn` so the closure can fan out
/// writes via the caller-provided writer abstraction.
pub type MessageSink<'a> = &'a mut dyn FnMut(&MessageHeader, &[u8]) -> Result<(), BoxedError>;

/// Type-erased, shared serializer closure. The closure takes a `GenericSubscriber`,
/// drains it via the right typed `InputSpan::<T>` (T captured statically in
/// the closure's monomorphization), serializes each drained message's value
/// into the caller-provided scratch buffer, and invokes `sink(header, body_bytes)`
/// for each message so the caller can write it to disk.
///
/// The sink is fallible (returns `Result<(), BoxedError>`) — IO failures
/// during writing short-circuit the iteration via `?` and surface in the
/// outer `MessageSinkError` return so the caller can record them on a
/// diagnostics channel.
pub type SerializerFn = Arc<
    dyn Fn(
            &mut dyn GenericSubscriber,
            &mut Vec<u8>,
            MessageSink<'_>,
        ) -> Result<(), MessageSinkError>
        + Send
        + Sync,
>;

/// Type-keyed registry of serializers. Build steps query this by `TypeId`
/// (obtained from `GenericPublisher::value_type_id`) to find a matching
/// `SerializerFn` for a publisher's value type.
pub struct ChannelRegistry {
    serializers: HashMap<TypeId, SerializerFn>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        ChannelRegistry {
            serializers: HashMap::new(),
        }
    }

    /// Register a serializer for `T`. Idempotent — calling twice with the
    /// same `T` overwrites the first, but since the closure is monomorphized
    /// identically there's no observable difference.
    pub fn register_loggable<T: 'static + Loggable>(&mut self) -> &mut Self {
        let tid = TypeId::of::<T>();
        let serializer: SerializerFn = Arc::new(
            |sub: &mut dyn GenericSubscriber, scratch: &mut Vec<u8>, sink: MessageSink<'_>| {
                let mut span = InputSpan::<T>::new_downcasted(sub);
                // drain_inputs yields ArenaReaderPtr<Message<T>>; `Deref`
                // gives us `&Message<T>` transparently inside the loop body.
                for msg in span.drain_inputs() {
                    let msg = &*msg;
                    scratch.clear();
                    msg.message
                        .serialize(scratch)
                        .map_err(MessageSinkError::Serialize)?;
                    sink(&msg.header, scratch).map_err(MessageSinkError::Sink)?;
                }
                Ok(())
            },
        );
        self.serializers.insert(tid, serializer);
        self
    }

    pub fn serializer_for(&self, type_id: TypeId) -> Option<SerializerFn> {
        self.serializers.get(&type_id).cloned()
    }

    /// Absorb another registry's entries into this one. Used by `TaskGraphBuilder`
    /// to merge per-CallbackBuilder registries into a single shared registry
    /// for build-step consumption.
    pub fn merge(&mut self, other: ChannelRegistry) {
        self.serializers.extend(other.serializers);
    }
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── MaybeRegister pattern ───────────────────────────────────────────────────
//
// Detects at compile time whether a type `T` implements `Loggable` and, if so,
// registers it. Works on stable Rust via the inherent-method-vs-blanket-trait
// trick:
//
//   * `MaybeRegister` is a trait with a no-op default `try_register`.
//   * A blanket `impl<T: ?Sized> MaybeRegister for T` covers every type, so
//     `Probe::<X>` always has *some* `try_register` candidate via the trait.
//   * An inherent `impl<T: 'static + Loggable> Probe<T>` overrides with the
//     real registration. Inherent methods beat trait methods during method
//     resolution *when the inherent impl's where-clause is satisfiable* —
//     i.e. when `T: Loggable`. When `T: !Loggable`, the inherent candidate is
//     filtered out and the blanket trait's no-op default applies.
//
// No `min_specialization` required.

pub trait MaybeRegister {
    fn try_register(&self, _registry: &mut ChannelRegistry) {}
}

impl<T: ?Sized> MaybeRegister for T {}

pub struct Probe<T>(PhantomData<T>);

impl<T> Probe<T> {
    /// Construct a `Probe<T>` for compile-time trait detection. The probe
    /// itself carries no runtime state; method resolution on `.try_register()`
    /// dispatches to either the inherent impl (when `T: 'static + Loggable`)
    /// or the blanket `MaybeRegister` impl's no-op default otherwise.
    pub fn new() -> Self {
        Probe(PhantomData)
    }
}

impl<T> Default for Probe<T> {
    fn default() -> Self {
        Probe::new()
    }
}

impl<T: 'static + Loggable> Probe<T> {
    pub fn try_register(&self, registry: &mut ChannelRegistry) {
        registry.register_loggable::<T>();
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, Default)]
    struct LoggableType {
        x: i32,
    }

    /// A type that does NOT implement Loggable (no Serialize/Deserialize).
    struct NonLoggableType;

    #[test]
    fn probe_registers_loggable_type() {
        let mut registry = ChannelRegistry::new();
        Probe::<LoggableType>::new().try_register(&mut registry);
        assert!(
            registry
                .serializer_for(TypeId::of::<LoggableType>())
                .is_some()
        );
    }

    #[test]
    fn probe_silently_skips_non_loggable_type() {
        let mut registry = ChannelRegistry::new();
        // This compiles only because the blanket trait MaybeRegister for T
        // provides a no-op default; the inherent try_register is filtered out
        // since NonLoggableType: !Loggable.
        use MaybeRegister as _;
        Probe::<NonLoggableType>::new().try_register(&mut registry);
        assert!(
            registry
                .serializer_for(TypeId::of::<NonLoggableType>())
                .is_none()
        );
    }
}
