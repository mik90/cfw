use crate::callback::CallbackNodeReadiness;
use crate::message::MessageHeader;
use crate::subscriber::SubscriberConfig;
use std::any::Any;
use std::sync::Arc;

pub struct QueueInfo {
    pub reader_size: usize,
    pub writer_size: usize,
}

pub trait GenericSubscriber {
    fn as_any(&mut self) -> &mut dyn std::any::Any;

    fn config(&self) -> &SubscriberConfig;

    fn config_mut(&mut self) -> &mut SubscriberConfig;

    fn able_to_run(&self) -> bool;

    fn requests_execution(&self) -> bool;

    fn drain_writer_to_reader(&self);

    fn queue_info(&self) -> QueueInfo;

    /// Clear buffered values before the Arena is dropped.
    /// Prevents ArenaPtrs from outliving their Arena allocators.
    fn cleanup_buffers(&self) {}

    /// Iterate the read buffer (after `drain_writer_to_reader`) yielding each
    /// message's typed header and type-erased payload value (a `&T` upcast to
    /// `&dyn Any`). The default no-op impl is used by subscribers that don't
    /// participate in logging.
    fn for_each_queued_input(&self, _f: &mut dyn FnMut(&MessageHeader, &dyn Any)) {}

    /// Inject the shared readiness bitmask and this subscriber's bit index.
    /// Called by CallbackNode::new_with after creating the bitmask Arc.
    fn set_readiness_state(&mut self, _state: Arc<CallbackNodeReadiness>, _bit_index: usize) {}

    /// Return the readiness state so that a connecting publisher can store it.
    fn readiness_state(&self) -> Option<(Arc<CallbackNodeReadiness>, usize)> {
        None
    }
}
