use std::any::TypeId;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::input::InputSpan;
use crate::loggable::{DeserializeError, Loggable, SerializeError};
use crate::message::MessageHeader;
use crate::pub_sub::ChannelName;

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

/// Type-erased deserializer closure: takes raw bytes and returns a
/// heap-allocated, type-erased value. Used by replay executors to reconstruct
/// typed messages from a log file before publishing them.
pub type DeserializerFn = Arc<
    dyn Fn(&[u8]) -> Result<Box<dyn std::any::Any + Send + Sync>, DeserializeError> + Send + Sync,
>;

/// Per-channel factory closure stored alongside a deserializer. Given a
/// channel name, creates a typed `Publisher<T>` (wrapped as
/// `GenericPublisher`) and a writer closure that accepts an `Any` value,
/// downcasts it to `T`, and publishes through `Output<'_, T>`.
pub type ChannelPublisherFactory =
    Arc<dyn Fn(ChannelName) -> (Box<dyn GenericPublisher>, ChannelPublisherWriter) + Send + Sync>;

/// Type-erased writer that publishes a heap-allocated value through a
/// `GenericPublisher`. Monomorphized per `T` at registration time so the
/// hot path avoids a runtime type check inside the loop.
pub type ChannelPublisherWriter =
    Arc<dyn Fn(&mut dyn GenericPublisher, Box<dyn std::any::Any + Send + Sync>) + Send + Sync>;

/// Type-keyed registry of serializers and deserializers.
/// Build steps query this by `TypeId` (obtained from `GenericPublisher::value_type_id`)
/// to find a matching `SerializerFn` for a publisher's value type, or a
/// `DeserializerFn` for replaying logged messages.
#[derive(Clone)]
pub struct ChannelRegistry {
    serializers: HashMap<TypeId, SerializerFn>,
    deserializers: HashMap<TypeId, DeserializerFn>,
    publisher_factories: HashMap<TypeId, ChannelPublisherFactory>,
    channels: HashMap<ChannelName, TypeId>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        ChannelRegistry {
            serializers: HashMap::new(),
            deserializers: HashMap::new(),
            publisher_factories: HashMap::new(),
            channels: HashMap::new(),
        }
    }

    /// Register a serializer for `T`. Idempotent — calling twice with the
    /// same `T` overwrites the first, but since the closure is monomorphized
    /// identically there's no observable difference.
    pub fn register_loggable<T: 'static + Loggable>(&mut self) -> &mut Self {
        self.register_serializer::<T>()
    }

    fn register_serializer<T: 'static + Loggable>(&mut self) -> &mut Self {
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
        self.serializers.insert(TypeId::of::<T>(), serializer);
        self
    }

    fn register_deserializer<T: 'static + Loggable<Context<'static> = ()> + Send + Sync>(
        &mut self,
    ) -> &mut Self {
        // Deserializer — uses the blanket serde (Context<'static> = ()) path
        let deserializer: DeserializerFn = Arc::new(|bytes: &[u8]| {
            let value = T::deserialize(bytes)?;
            Ok(Box::new(value) as Box<dyn std::any::Any + Send + Sync>)
        });
        self.deserializers.insert(TypeId::of::<T>(), deserializer);
        self
    }

    pub fn register_publisher_factory<
        T: 'static + Loggable<Context<'static> = ()> + Send + Sync,
    >(
        &mut self,
    ) -> &mut Self {
        // Publisher factory: creates a Publisher<T> + writer for replay.
        let factory: ChannelPublisherFactory = Arc::new(move |channel_name: ChannelName| {
            use crate::output::Output;
            use crate::publisher::{Publisher, PublisherConfig};

            let publisher: Box<dyn GenericPublisher> =
                Box::new(Publisher::<T>::new(PublisherConfig {
                    capacity: 1,
                    channel_name,
                }));
            let writer: ChannelPublisherWriter = Arc::new(
                |pub_ref: &mut dyn GenericPublisher, val: Box<dyn std::any::Any + Send + Sync>| {
                    let typed = pub_ref
                        .as_any()
                        .downcast_mut::<Publisher<T>>()
                        .expect("ReplayTask: publisher type mismatch");
                    let val = val
                        .downcast::<T>()
                        .expect("ReplayTask: value type mismatch");
                    let output = Output::new_with_factory(typed, |slot| {
                        slot.write(*val);
                    });
                    output.send();
                },
            );
            (publisher, writer)
        });
        self.publisher_factories.insert(TypeId::of::<T>(), factory);
        self
    }

    /// Register a channel's value type: stores a serializer, a deserializer,
    /// and a `channel_name → TypeId` mapping so replay executors can discover
    /// what type a given channel carries and how to deserialize its logged
    /// messages.
    ///
    /// Requires `T: Loggable<Context<'static> = ()>` — types with a
    /// non-trivial deserialization context (e.g. `ForwardedMessage`) must be
    /// denylisted during replay and are not supported here.
    pub fn register_channel<T: 'static + Loggable<Context<'static> = ()> + Send + Sync>(
        &mut self,
        channel: ChannelName,
    ) -> &mut Self {
        self.register_serializer::<T>();

        self.register_deserializer::<T>();

        self.register_publisher_factory::<T>();

        self.channels.insert(channel, TypeId::of::<T>());
        self
    }

    pub fn serializer_for(&self, type_id: TypeId) -> Option<SerializerFn> {
        // TODO: expose reference to function instead of copied Arc
        self.serializers.get(&type_id).cloned()
    }

    /// Look up the `TypeId` registered for the given channel name.
    /// Returns `None` if the channel was never registered.
    pub fn channel_type(&self, channel: &str) -> Option<TypeId> {
        // TODO: expose reference to function instead of copied Arc
        self.channels.get(channel).copied()
    }

    /// Look up the deserializer registered for the given type.
    /// Returns `None` if the type was never registered.
    pub fn deserializer_for(&self, type_id: TypeId) -> Option<DeserializerFn> {
        // TODO: expose reference to function instead of copied Arc
        self.deserializers.get(&type_id).cloned()
    }

    /// Look up the publisher factory registered for the given type.
    /// Returns `None` if the type was never registered.
    pub fn channel_publisher_factory(&self, type_id: TypeId) -> Option<ChannelPublisherFactory> {
        // TODO: expose reference to function instead of copied Arc
        self.publisher_factories.get(&type_id).cloned()
    }

    /// Absorb another registry's entries into this one. Used by `TaskGraphBuilder`
    /// to merge per-CallbackBuilder registries into a single shared registry
    /// for build-step consumption.
    pub fn merge(&mut self, other: ChannelRegistry) {
        self.serializers.extend(other.serializers);
        self.deserializers.extend(other.deserializers);
        self.publisher_factories.extend(other.publisher_factories);
        self.channels.extend(other.channels);
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
        use MaybeRegister as _;
        // This compiles only because the blanket trait MaybeRegister for T
        // provides a no-op default; the inherent try_register is filtered out
        // since NonLoggableType: !Loggable.
        Probe::<NonLoggableType>::new().try_register(&mut registry);
        assert!(
            registry
                .serializer_for(TypeId::of::<NonLoggableType>())
                .is_none()
        );
    }
}
