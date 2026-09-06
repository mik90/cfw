use crate::callback::SubscriberReadiness;
use crate::generic_subscriber::{GenericSubscriber, QueueInfo};
use crate::message::MessageHeader;
use crate::subscriber::SubscriberConfig;
use iceoryx2::port::subscriber::Subscriber as Iceoryx2Subscriber;
use iceoryx2::prelude::*;
use std::fmt::Debug;

/// Keep our headers internally owned so that the pub/sub system is always able to use them
pub type Iceoryx2UserHeader = ();

/// Iceoryx2 has more constraints than we do
pub struct Iceoryx2SubscriberWrapper<T: Debug + ZeroCopySend + ?Sized + 'static> {
    config: SubscriberConfig,
    // We're using ipc_threadsafe to avoid issues around Send/Sync, although only one thread will ever use the IPC service
    iceoryx2_subscriber: Iceoryx2Subscriber<ipc_threadsafe::Service, T, Iceoryx2UserHeader>,
}

impl<T: Debug + ZeroCopySend + ?Sized + 'static> GenericSubscriber
    for Iceoryx2SubscriberWrapper<T>
{
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn able_to_run(&self) -> bool {
        if self.config.is_optional {
            true
        } else {
            todo!()
        }
    }

    fn config(&self) -> &SubscriberConfig {
        &self.config
    }

    fn config_mut(&mut self) -> &mut SubscriberConfig {
        &mut self.config
    }

    fn requests_execution(&self) -> bool {
        if self.config.is_trigger {
            todo!()
        } else {
            false
        }
    }

    fn drain_writer_to_reader(&self) {
        todo!()
    }

    fn queue_info(&self) -> QueueInfo {
        todo!()
    }

    fn cleanup_buffers(&self) {
        todo!()
    }

    fn set_readiness_state(&mut self, _state: SubscriberReadiness) {
        todo!()
    }

    fn readiness_state(&self) -> Option<SubscriberReadiness> {
        todo!()
    }

    fn for_each_queued_input(&self, _f: &mut dyn FnMut(&MessageHeader, &dyn std::any::Any)) {
        todo!()
    }
}
