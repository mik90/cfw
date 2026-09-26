use crate::callback::SubscriberReadiness;
use crate::channel_registry::BoxedError;
use crate::message::MessageHeader;
use crate::pub_sub_factory::Iox2EndpointInfo;
use crate::subscriber::SubscriberConfig;
use crate::task_graph_builder::TaskGraphBuildError;
use std::any::Any;

pub struct QueueInfo {
    pub reader_size: usize,
    pub writer_size: usize,
}

pub trait GenericSubscriber: Send {
    /// Report this endpoint for graph-wide transport and payload validation.
    fn iox2_find_endpoints(
        &self,
        _add: &mut dyn FnMut(Iox2EndpointInfo) -> Result<(), TaskGraphBuildError>,
    ) -> Result<(), TaskGraphBuildError> {
        Ok(())
    }

    /// Open this endpoint's iceoryx2 resources (service ports, notifiers) after
    /// graph validation and connection. Only meaningful for iox2-backed
    /// subscribers; the default no-op keeps every other implementation
    /// transport-neutral.
    #[cfg(feature = "iceoryx2")]
    fn iox2_open(
        &mut self,
        _ctx: &mut dyn crate::iox2::Iox2OpenCtx,
    ) -> Result<(), TaskGraphBuildError> {
        Ok(())
    }

    /// Hand an event subscriber's listener and registration to the live executor.
    /// The default means this subscriber is not an event input.
    #[cfg(feature = "iceoryx2")]
    fn iox2_take_event_registration(
        &mut self,
        _ctx: &mut dyn crate::iox2::Iox2OpenCtx,
    ) -> Result<Option<crate::iox2::Iox2EventRegistration>, String> {
        Ok(None)
    }

    /// Create a simulation-owned publisher for injecting data into this input's channel.
    /// The default means this subscriber is not an iox2 data input.
    #[cfg(feature = "iceoryx2")]
    fn iox2_create_simulation_publisher(
        &self,
        _ctx: &mut dyn crate::iox2::Iox2OpenCtx,
    ) -> Result<Option<Box<dyn crate::iox2::Iox2SyntheticPublisher>>, String> {
        Ok(None)
    }

    /// Provide a typed iox2 publisher factory for log replay without opening its service.
    #[cfg(feature = "iceoryx2")]
    fn iox2_replay_publisher_factory(
        &self,
    ) -> Option<crate::channel_registry::Iox2ReplayPublisherFactory> {
        None
    }
    fn as_any(&mut self) -> &mut dyn std::any::Any;

    fn config(&self) -> &SubscriberConfig;

    fn config_mut(&mut self) -> &mut SubscriberConfig;

    fn able_to_run(&self) -> bool;

    fn requests_execution(&self) -> bool;

    fn drain_writer_to_reader(&self);

    fn queue_info(&self) -> QueueInfo;

    /// Whether the callback will see data from this subscriber after the next
    /// write→read drain: pending data in the write queue, or a retained value
    /// in the read buffer. Used to gate nodes on their required inputs.
    fn has_data_available(&self) -> bool {
        let info = self.queue_info();
        info.reader_size > 0 || info.writer_size > 0
    }

    /// Clear buffered values before the Arena is dropped.
    /// Prevents ArenaPtrs from outliving their Arena allocators.
    fn cleanup_buffers(&self) {}

    /// Iterate the read buffer (after `drain_writer_to_reader`) yielding each
    /// message's typed header and type-erased payload value (a `&T` upcast to
    /// `&dyn Any`). The default no-op impl is used by subscribers that don't
    /// participate in logging.
    fn for_each_queued_input(&self, _f: &mut dyn FnMut(&MessageHeader, &dyn Any)) {}

    /// Consume and clear inputs for the logging path. Every implementation
    /// must choose explicitly: invoke `f` for loggable messages, or provide a
    /// documented no-op when draining does not apply. A silent default could
    /// otherwise make a hand-written subscriber disappear from logs unnoticed.
    fn drain_queued_inputs(
        &mut self,
        _f: &mut dyn FnMut(&MessageHeader, &dyn Any) -> Result<(), BoxedError>,
    ) -> Result<(), BoxedError>;

    /// Inject this subscriber's readiness role (gating bit, or bit-less
    /// optional-trigger handle). Called by CallbackNode::new_with after
    /// creating the shared readiness state.
    fn set_readiness_state(&mut self, _state: SubscriberReadiness) {}

    /// Return the readiness state so that a connecting publisher can store it.
    fn readiness_state(&self) -> Option<SubscriberReadiness> {
        None
    }
}
