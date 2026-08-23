use std::any::{Any, TypeId};

use crate::generic_subscriber::GenericSubscriber;
use crate::message::MessageHeader;
use crate::pub_sub::ChannelName;
use crate::publisher::PublisherConfig;
use crate::scheduling::ReadyNodeSink;
use crate::subscriber::SubscriberConfig;
use crate::time::FrameworkTime;

#[derive(Debug)]
pub struct ConnectionTypeMismatch {}

pub trait GenericPublisher: Send {
    fn as_any(&mut self) -> &mut dyn Any;

    fn config(&self) -> &PublisherConfig;

    fn config_mut(&mut self) -> &mut PublisherConfig;

    /// The channel names this publisher forwards onto.
    ///
    /// For a plain [`Publisher`](crate::publisher::Publisher) this is always
    /// empty. For a [`ForwardingPublisher`](crate::publisher::ForwardingPublisher)
    /// it lists the *output* channels — the channels on which it publishes
    /// [`ForwardedMessage`](crate::forwarded_message::ForwardedMessage)
    /// values that reference messages from one of its forwardable input
    /// channels.
    ///
    /// The graph builder uses this to size arenas correctly: every subscriber
    /// on one of the listed channels contributes `2 * capacity` slots to the
    /// forwarding publisher's arena, because each forwarded message is cloned
    /// into the subscriber's write queue *and* read buffer while still
    /// pointing at the forwarding publisher's arena.
    fn forwarded_channels(&self) -> &[ChannelName];

    fn flush_loaned_values(&mut self, timestamp: FrameworkTime, sink: &mut dyn ReadyNodeSink);

    /// Flush sent loans, stamping each with `timestamp`, and invoke `hook` with
    /// each published message's header as it is committed. Lets an executor
    /// observe published headers (which are only valid once stamped here) for
    /// execution logging. The default calls [`flush_loaned_values`] (no hook),
    /// so publishers that don't participate in logging are unaffected.
    ///
    /// [`flush_loaned_values`]: GenericPublisher::flush_loaned_values
    fn flush_loaned_values_logged(
        &mut self,
        timestamp: FrameworkTime,
        sink: &mut dyn ReadyNodeSink,
        _hook: &mut dyn FnMut(&MessageHeader),
    ) {
        self.flush_loaned_values(timestamp, sink);
    }

    fn allocate_arena(&mut self);

    fn increase_arena_size(&mut self, additional_capacity: usize);

    fn connect_to_subscriber(
        &mut self,
        subscriber: &mut dyn GenericSubscriber,
    ) -> Result<(), ConnectionTypeMismatch>;

    /// Iterate over sent-but-not-yet-flushed outputs.
    /// The default no-op impl is used by publishers that don't participate in logging.
    fn for_each_pending_output(&self, _f: &mut dyn FnMut(&MessageHeader, &dyn Any)) {}

    /// Construct a subscriber whose `T` matches this publisher's value type.
    /// Returns `None` for publishers that don't support introspection-based
    /// subscription (the default). Logging build steps use this to wire up
    /// a `ChannelLogger` without statically knowing `T`.
    ///
    /// - A plain [`Publisher<T>`](crate::publisher::Publisher) returns
    ///   [`Subscriber<T>`](crate::subscriber::Subscriber).
    /// - A [`ForwardingPublisher<T, F>`](crate::publisher::ForwardingPublisher)
    ///   returns `Subscriber<ForwardedMessage<T, F>>` — the type its
    ///   downstream consumers subscribe with.
    ///
    /// Replay's output-capture path (`PublisherCapture::connect_from`) relies
    /// on this so capture subscribers match the publisher's real value type
    /// without static knowledge of `T`.
    fn build_matching_subscriber(
        &self,
        _config: SubscriberConfig,
    ) -> Option<Box<dyn GenericSubscriber>> {
        None
    }

    /// The `TypeId` of the publisher's payload type `T` (or, for forwarding
    /// publishers, `ForwardedMessage<T, F>`). Logging build steps look up
    /// serializers in a `ChannelRegistry` by this id. The default returns
    /// the `TypeId` of `()`, which no user type will match — safely opting
    /// un-introspectable publishers out of logging.
    fn value_type_id(&self) -> TypeId {
        TypeId::of::<()>()
    }
}
