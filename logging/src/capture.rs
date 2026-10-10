use crate::BoxedLogError;
use task::{
    ChannelPlan, EndpointBindings, Subscriber, SubscriberKey, SubscriberPolicy, loggable::Loggable,
    message::MessageHeader,
};

/// Register before allocating storage so capture buffers are budgeted in arenas.
pub struct CapturePlan<T> {
    key: SubscriberKey<T>,
}
impl<T> CapturePlan<T> {
    pub fn declare(plan: &mut ChannelPlan<T>, capacity: usize) -> Self {
        Self {
            key: plan.subscriber_with_policy(
                capacity,
                SubscriberPolicy {
                    trigger: false,
                    keep_across_runs: true,
                },
            ),
        }
    }
    pub fn bind<'a>(
        self,
        bindings: &EndpointBindings<'a, T>,
    ) -> Result<Capture<'a>, task::EndpointError>
    where
        T: Loggable + Send + Sync + 'a,
    {
        Ok(Capture::native(bindings.take_subscriber(&self.key)?))
    }
}
/// Type-erased capture retaining the storage lifetime, never requiring Clone or Any.
pub struct Capture<'a> {
    channel: String,
    drain: Box<dyn task::automatic::capture::SerializedCapture + 'a>,
}
impl<'a> Capture<'a> {
    pub fn native<T: Loggable + Send + Sync + 'a>(subscriber: Subscriber<'a, T>) -> Self {
        Self::from_serialized(Box::new(task::automatic::capture::NativeCapture::new(
            subscriber,
        )))
    }
    pub(crate) fn from_serialized(
        drain: Box<dyn task::automatic::capture::SerializedCapture + 'a>,
    ) -> Self {
        Self {
            channel: drain.channel().into(),
            drain,
        }
    }
    pub fn channel(&self) -> &str {
        &self.channel
    }
    #[cfg(feature = "serde")]
    pub(crate) fn loss(&self) -> crate::incompleteness::CaptureLoss {
        let (writer_drops, reader_drops, receive_errors) = self.drain.loss_counts();
        crate::incompleteness::CaptureLoss {
            channel: self.channel.clone(),
            writer_drops: writer_drops as u64,
            reader_drops: reader_drops as u64,
            receive_errors: receive_errors as u64,
        }
    }
    pub fn drain(
        &mut self,
        mut visit: impl FnMut(MessageHeader, &[u8]) -> Result<(), BoxedLogError>,
    ) -> Result<(), BoxedLogError> {
        self.drain.drain(&mut visit)
    }
    pub fn drain_to_vec(&mut self) -> Result<Vec<(MessageHeader, Vec<u8>)>, BoxedLogError> {
        let mut messages = Vec::new();
        self.drain(|header, body| {
            messages.push((header, body.to_vec()));
            Ok(())
        })?;
        Ok(messages)
    }
}

#[cfg(feature = "iceoryx2")]
mod ipc {
    use super::*;
    use task::iox2::Iox2Subscriber;
    // These bounds are expressed via the subscriber's concrete type; transport
    // payload requirements match task's public middleware endpoint API.
    impl<'a> Capture<'a> {
        pub fn ipc<T>(subscriber: Iox2Subscriber<T>) -> Self
        where
            T: Loggable + std::fmt::Debug + iceoryx2::prelude::ZeroCopySend + Send + Sync + 'static,
        {
            Self::from_serialized(Box::new(task::automatic::capture::IpcCapture(subscriber)))
        }
    }
}
