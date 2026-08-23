use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::input::InputSpan;
use crate::loggable::{DeserializeError, ForwardedMessageContext, Loggable, SerializeError};
use crate::message::MessageHeader;
use crate::pub_sub::ChannelName;

#[cfg(feature = "serde")]
use crate::forwarded_message::ForwardedMessage;

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
///
/// The returned value is not `Send`/`Sync`: deserialized messages are created
/// and consumed on the same replay thread. Only the closure itself (which
/// captures nothing) needs `Send + Sync` so the registry can cross threads.
pub type DeserializerFn =
    Arc<dyn Fn(&[u8]) -> Result<Box<dyn Any>, DeserializeError> + Send + Sync>;

/// Type-erased deserializer for forwarded-message channels.
///
/// Same contract as [`DeserializerFn`], but additionally receives a
/// [`ForwardedMessageContext`] — the source channel's logged payloads — so it
/// can resolve the forwarded message's referenced payload by header. Kept
/// separate from [`DeserializerFn`] because only forwarded channels need a
/// context; plain deserializers shouldn't have to acknowledge one.
pub type ForwardedDeserializerFn = Arc<
    dyn Fn(&[u8], &ForwardedMessageContext) -> Result<Box<dyn Any>, DeserializeError> + Send + Sync,
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
///
/// The value is not `Send`/`Sync` (see [`DeserializerFn`]); it is produced
/// and consumed on the same replay thread.
pub type ChannelPublisherWriter =
    Arc<dyn Fn(&mut dyn GenericPublisher, Box<dyn Any>) + Send + Sync>;

/// How a forwarded channel relates to its source channel.
///
/// A forwarded channel carries [`ForwardedMessage<T, F>`] values whose payload
/// (`F`) lives on the *source* channel. Replay uses this mapping to build the
/// [`ReplayMessageLog<F>`] context that resolves forwarded payloads by header.
///
/// [`ForwardedMessage<T, F>`]: crate::forwarded_message::ForwardedMessage
/// [`ReplayMessageLog<F>`]: crate::loggable::ReplayMessageLog
#[derive(Debug, Clone)]
pub struct ForwardedChannelInfo {
    /// The channel that carries the forwarded payload type `F`.
    pub source_channel: ChannelName,
    /// The `TypeId` of the forwarded payload type `F`.
    pub forwarded_data_type_id: TypeId,
}

/// Type-keyed registry of serializers and deserializers.
/// Build steps query this by `TypeId` (obtained from `GenericPublisher::value_type_id`)
/// to find a matching `SerializerFn` for a publisher's value type, or a
/// `DeserializerFn` for replaying logged messages.
#[derive(Clone)]
pub struct ChannelRegistry {
    serializers: HashMap<TypeId, SerializerFn>,
    deserializers: HashMap<TypeId, DeserializerFn>,
    forwarded_deserializers: HashMap<TypeId, ForwardedDeserializerFn>,
    publisher_factories: HashMap<TypeId, ChannelPublisherFactory>,
    channels: HashMap<ChannelName, TypeId>,
    forwarded_channels: HashMap<ChannelName, ForwardedChannelInfo>,
}

impl ChannelRegistry {
    /// A fresh registry, pre-populated with the framework's own
    /// [`ExecutionLogMessage`](crate::execution_log::ExecutionLogMessage)
    /// channel so build steps and replay executors can always resolve it. The
    /// probe is a no-op when the `serde` feature is off (and thus
    /// `ExecutionLogMessage` isn't loggable).
    pub fn new() -> Self {
        let mut registry = ChannelRegistry {
            serializers: HashMap::new(),
            deserializers: HashMap::new(),
            forwarded_deserializers: HashMap::new(),
            publisher_factories: HashMap::new(),
            channels: HashMap::new(),
            forwarded_channels: HashMap::new(),
        };
        Probe::<crate::execution_log::ExecutionLogMessage>::new().try_register_channel(
            &mut registry,
            crate::execution_log::EXECUTION_LOG_CHANNEL.into(),
        );
        registry
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
            Ok(Box::new(value) as Box<dyn Any>)
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
            let writer: ChannelPublisherWriter =
                Arc::new(|pub_ref: &mut dyn GenericPublisher, val: Box<dyn Any>| {
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
                });
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
    /// non-trivial deserialization context, like
    /// [`ForwardedMessage`](crate::forwarded_message::ForwardedMessage), use
    /// [`register_forwarded_channel`](Self::register_forwarded_channel)
    /// instead.
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

    /// Register a forwarded channel's value type.
    ///
    /// A forwarded channel carries [`ForwardedMessage<T, F>`] values: the extra
    /// payload `T` plus a reference to a message on `source_channel` (whose
    /// payload type is `F`). Registration provides:
    ///
    /// - a serializer for `ForwardedMessage<T, F>` (output capture + logging),
    /// - a deserializer that resolves the forwarded payload via a
    ///   [`ForwardedMessageContext`] built from `source_channel`'s logged
    ///   messages, and
    /// - a publisher factory that hydrates `Subscriber<ForwardedMessage<T, F>>`.
    ///
    /// The forwarding node's own `ForwardableSubscriber<F>` input is hydrated
    /// normally — register `source_channel` with [`register_channel`] too.
    ///
    /// [`ForwardedMessage<T, F>`]: crate::forwarded_message::ForwardedMessage
    #[cfg(feature = "serde")]
    pub fn register_forwarded_channel<
        T: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
        F: Clone + Send + Sync + 'static,
    >(
        &mut self,
        forwarded_channel: ChannelName,
        source_channel: ChannelName,
    ) -> &mut Self {
        let forwarded_type_id = TypeId::of::<ForwardedMessage<T, F>>();

        self.register_serializer::<ForwardedMessage<T, F>>();

        let forwarded_deserializer: ForwardedDeserializerFn =
            Arc::new(|bytes: &[u8], context: &ForwardedMessageContext| {
                let log = context.to_log::<F>();
                let value = ForwardedMessage::<T, F>::deserialize_with_ctx(bytes, &log)?;
                Ok(Box::new(value) as Box<dyn Any>)
            });
        self.forwarded_deserializers
            .insert(forwarded_type_id, forwarded_deserializer);

        let factory: ChannelPublisherFactory = Arc::new(move |channel_name: ChannelName| {
            use crate::output::Output;
            use crate::publisher::{Publisher, PublisherConfig};

            let publisher: Box<dyn GenericPublisher> =
                Box::new(Publisher::<ForwardedMessage<T, F>>::new(PublisherConfig {
                    capacity: 1,
                    channel_name,
                }));
            let writer: ChannelPublisherWriter =
                Arc::new(|pub_ref: &mut dyn GenericPublisher, val: Box<dyn Any>| {
                    let typed = pub_ref
                        .as_any()
                        .downcast_mut::<Publisher<ForwardedMessage<T, F>>>()
                        .expect("ReplayTask: forwarded publisher type mismatch");
                    let val = val
                        .downcast::<ForwardedMessage<T, F>>()
                        .expect("ReplayTask: forwarded value type mismatch");
                    let output = Output::new_with_factory(typed, |slot| {
                        slot.write(*val);
                    });
                    output.send();
                });
            (publisher, writer)
        });
        self.publisher_factories.insert(forwarded_type_id, factory);

        self.channels
            .insert(forwarded_channel.clone(), forwarded_type_id);
        self.forwarded_channels.insert(
            forwarded_channel,
            ForwardedChannelInfo {
                source_channel,
                forwarded_data_type_id: TypeId::of::<F>(),
            },
        );
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

    /// Look up the forwarded-channel mapping for `channel`. Returns `None` for
    /// ordinary channels or unknown channel names.
    pub fn forwarded_channel_info(&self, channel: &str) -> Option<&ForwardedChannelInfo> {
        self.forwarded_channels.get(channel)
    }

    /// Look up the deserializer registered for the given type.
    /// Returns `None` if the type was never registered.
    pub fn deserializer_for(&self, type_id: TypeId) -> Option<DeserializerFn> {
        // TODO: expose reference to function instead of copied Arc
        self.deserializers.get(&type_id).cloned()
    }

    /// Look up the forwarded-message deserializer registered for the given
    /// type. Returns `None` if the type was never registered as a forwarded
    /// channel.
    pub fn forwarded_deserializer_for(&self, type_id: TypeId) -> Option<ForwardedDeserializerFn> {
        // TODO: expose reference to function instead of copied Arc
        self.forwarded_deserializers.get(&type_id).cloned()
    }

    /// Look up the publisher factory registered for the given type.
    /// Returns `None` if the type was never registered.
    pub fn channel_publisher_factory(&self, type_id: TypeId) -> Option<ChannelPublisherFactory> {
        // TODO: expose reference to function instead of copied Arc
        self.publisher_factories.get(&type_id).cloned()
    }

    /// Number of registered serializer types. Used for diagnostics.
    pub fn serializer_count(&self) -> usize {
        self.serializers.len()
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
    fn try_register_channel(&self, _registry: &mut ChannelRegistry, _channel: ChannelName) {}
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

impl<T: 'static + Loggable<Context<'static> = ()> + Send + Sync> Probe<T> {
    pub fn try_register_channel(&self, registry: &mut ChannelRegistry, channel: ChannelName) {
        registry.register_channel::<T>(channel);
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
