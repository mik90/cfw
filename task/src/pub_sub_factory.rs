use std::any::TypeId;

/// Classification of a graph endpoint for transport validation and iox2
/// service sizing. Plain data only — this must compile without the iceoryx2
/// feature, so event ids are stored as `usize` (`EventId::as_value()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndpointKind {
    /// Native subscriber endpoint.
    NativeSub,
    /// Native publisher endpoint.
    NativePub,
    /// Native forwardable input endpoint.
    NativeForwardedSub,
    /// Native forwarding output endpoint.
    NativeForwardingPub,
    /// iox2 data input with its port buffer capacity.
    Iox2DataSub { capacity: usize },
    /// iox2 data output with its loan capacity, whether sends also notify the
    /// channel's event service, and the notification event id.
    Iox2DataPub {
        capacity: usize,
        notify_on_send: bool,
        event_id: usize,
    },
    /// iox2 event input endpoint.
    Iox2EventSub,
    /// iox2 notifier endpoint with an integer event identifier.
    Iox2Notifier { event_id: usize },
}

impl EndpointKind {
    /// Whether this endpoint is served by an iceoryx2 transport.
    pub fn is_iox2(&self) -> bool {
        matches!(
            self,
            EndpointKind::Iox2DataSub { .. }
                | EndpointKind::Iox2DataPub { .. }
                | EndpointKind::Iox2EventSub
                | EndpointKind::Iox2Notifier { .. }
        )
    }
}

/// Plain-data description of a channel endpoint, usable without iceoryx2.
#[derive(Clone, Debug)]
pub struct Iox2EndpointInfo {
    /// Channel name as declared in the graph.
    pub channel: String,
    /// Endpoint transport and role.
    pub kind: EndpointKind,
    /// Payload type for data endpoints, absent for event-only endpoints.
    pub payload_type: Option<TypeId>,
}
