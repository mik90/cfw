//! Graph input sources that decode recorded payloads and publish them through
//! bound endpoints. Replay drivers control when each source injects a message.
use crate::BoxedLogError;
use task::{
    ChannelPlan, EndpointBindings, Publisher, PublisherKey, loggable::Loggable,
    message::MessageHeader,
};

pub struct ReplaySourcePlan<T> {
    key: PublisherKey<T>,
}
impl<T> ReplaySourcePlan<T> {
    pub fn declare(plan: &mut ChannelPlan<T>, capacity: usize) -> Self {
        Self {
            key: plan.publisher(capacity),
        }
    }
    pub fn bind<'a>(
        self,
        bindings: &EndpointBindings<'a, T>,
    ) -> Result<ReplaySource<'a>, task::EndpointError>
    where
        T: Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        Ok(ReplaySource::native(bindings.take_publisher(&self.key)?))
    }
    pub fn bind_with_decoder<'a>(
        self,
        bindings: &EndpointBindings<'a, T>,
        decode: impl FnMut(&[u8]) -> Result<T, BoxedLogError> + Send + 'a,
    ) -> Result<ReplaySource<'a>, task::EndpointError>
    where
        T: Send + Sync + 'a,
    {
        Ok(ReplaySource::with_decoder(
            bindings.take_publisher(&self.key)?,
            decode,
        ))
    }
}
type Inject<'a> = Box<dyn FnMut(MessageHeader, &[u8]) -> Result<(), BoxedLogError> + Send + 'a>;
pub struct ReplaySource<'a> {
    channel: String,
    payload_type: &'static str,
    inject: Inject<'a>,
}
impl<'a> ReplaySource<'a> {
    /// Publish silently; replay drivers inject counted events separately.
    #[cfg(feature = "iceoryx2")]
    pub fn ipc<T>(publisher: task::iox2::Iox2Publisher<T>) -> Self
    where
        T: std::fmt::Debug + iceoryx2::prelude::ZeroCopySend + Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        Self {
            channel: publisher.channel_name().into(),
            payload_type: std::any::type_name::<T>(),
            inject: Box::new(move |header, bytes| {
                publisher
                    .publish_with_header(header, T::deserialize(bytes)?)
                    .map_err(|e| -> BoxedLogError {
                        format!("IPC replay publication failed: {e:?}").into()
                    })
            }),
        }
    }
    pub fn native<T>(publisher: Publisher<'a, T>) -> Self
    where
        T: Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        Self::with_decoder(publisher, T::deserialize)
    }
    pub fn with_decoder<T: Send + Sync + 'a>(
        mut publisher: Publisher<'a, T>,
        mut decode: impl FnMut(&[u8]) -> Result<T, BoxedLogError> + Send + 'a,
    ) -> Self {
        Self {
            channel: publisher.channel_name().into(),
            payload_type: std::any::type_name::<T>(),
            inject: Box::new(move |header, bytes| {
                publisher
                    .publish_with_header(header, decode(bytes)?)
                    .map_err(|e| -> BoxedLogError {
                        format!("replay publication failed: {e:?}").into()
                    })?;
                Ok(())
            }),
        }
    }
    pub fn channel(&self) -> &str {
        &self.channel
    }
    pub fn payload_type(&self) -> &str {
        self.payload_type
    }
    #[cfg(feature = "iceoryx2")]
    pub fn ipc_input<T>(mut input: task::iox2::Iox2ReplayInput<T>) -> Self
    where
        T: Send + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        Self {
            channel: input.channel_name().into(),
            payload_type: std::any::type_name::<T>(),
            inject: Box::new(move |header, bytes| {
                input
                    .inject(header, T::deserialize(bytes)?)
                    .map_err(|e| -> BoxedLogError {
                        format!("IPC replay input failed: {e:?}").into()
                    })
            }),
        }
    }
    pub fn inject(&mut self, header: MessageHeader, bytes: &[u8]) -> Result<(), BoxedLogError> {
        (self.inject)(header, bytes)
    }
}
