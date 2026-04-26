use crate::callback::CallbackReadiness;
use crate::subscriber::SubscriberConfig;
use std::sync::Arc;

pub struct QueueInfo {
    pub reader_size: usize,
    pub writer_size: usize,
}

pub trait GenericSubscriber {
    fn as_any(&mut self) -> &mut dyn std::any::Any;

    fn get_config(&self) -> &SubscriberConfig;

    fn get_config_mut(&mut self) -> &mut SubscriberConfig;

    fn able_to_run(&self) -> bool;

    fn requests_execution(&self) -> bool;

    fn drain_writer_to_reader(&self);

    fn get_queue_info(&self) -> QueueInfo;

    /// Clear buffered values before the Arena is dropped.
    /// Prevents ArenaPtrs from outliving their Arena allocators.
    fn cleanup_buffers(&self) {}

    /// Iterate the read buffer (after `drain_writer_to_reader`) yielding each message's
    /// header and value. The default no-op impl is used by subscribers that don't
    /// participate in logging.
    fn for_each_queued_input(&self, _f: &mut dyn FnMut(&dyn std::any::Any)) {}

    /// Inject the shared readiness bitmask and this subscriber's bit index.
    /// Called by ConnectedCallback::new_with after creating the bitmask Arc.
    fn set_readiness_state(&mut self, _state: Arc<CallbackReadiness>, _bit_index: usize) {}

    /// Return the readiness state so that a connecting publisher can store it.
    fn get_readiness_state(&self) -> Option<(Arc<CallbackReadiness>, usize)> {
        None
    }
}
