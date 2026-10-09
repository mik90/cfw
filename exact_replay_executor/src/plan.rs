use crate::ReplayError;
use logging::{BoxedLogError, PortCapture, ReplaySource};
use std::collections::BTreeMap;
use task::{
    ChannelPlan, EndpointBindings, PublisherKey, StorageError, SubscriberKey, loggable::Loggable,
    message::MessageHeader,
};

type Decoder<'a, T> = std::sync::Arc<dyn Fn(&[u8]) -> Result<T, BoxedLogError> + Send + Sync + 'a>;
/// Bounded arena storage for forwarded-source resolution, declared in the same
/// source channel plan before allocating the graph.
pub struct SourceCachePlan<T> {
    publisher: PublisherKey<T>,
    subscriber: SubscriberKey<T>,
    capacity: usize,
}
impl<T> SourceCachePlan<T> {
    pub fn declare(plan: &mut ChannelPlan<T>, capacity: usize) -> Result<Self, StorageError> {
        let publisher = plan.publisher(1);
        plan.reserve_retained(&publisher, capacity)?;
        let subscriber = plan.subscriber(1);
        plan.restrict_subscriber_sources(&subscriber, std::slice::from_ref(&publisher))?;
        Ok(Self {
            publisher,
            subscriber,
            capacity,
        })
    }
    pub fn bind<'a>(
        self,
        bindings: &EndpointBindings<'a, T>,
    ) -> Result<SourceCache<'a, T>, task::EndpointError>
    where
        T: Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        self.bind_with_decoder(bindings, T::deserialize)
    }
    pub fn bind_with_decoder<'a>(
        self,
        bindings: &EndpointBindings<'a, T>,
        decode: impl Fn(&[u8]) -> Result<T, BoxedLogError> + Send + Sync + 'a,
    ) -> Result<SourceCache<'a, T>, task::EndpointError>
    where
        T: Send + Sync + 'a,
    {
        Ok(SourceCache {
            data: std::sync::Arc::new(std::sync::Mutex::new(CacheData {
                publisher: bindings.take_publisher(&self.publisher)?,
                subscriber: bindings.take_subscriber(&self.subscriber)?,
                messages: Vec::new(),
                capacity: self.capacity,
            })),
            decode: std::sync::Arc::new(decode),
        })
    }
}
struct CacheData<'a, T> {
    publisher: task::Publisher<'a, T>,
    subscriber: task::Subscriber<'a, T>,
    messages: Vec<base::arena::ArenaReaderPtr<'a, task::message::Message<T>>>,
    capacity: usize,
}
impl<'a, T> task::loggable::MessageLog<'a, T> for CacheData<'a, T> {
    fn lookup(
        &self,
        header: &MessageHeader,
    ) -> Option<base::arena::ArenaReaderPtr<'a, task::message::Message<T>>> {
        self.messages.iter().find(|m| m.header == *header).cloned()
    }
}
pub struct SourceCache<'a, T> {
    data: std::sync::Arc<std::sync::Mutex<CacheData<'a, T>>>,
    decode: Decoder<'a, T>,
}
impl<T> Clone for SourceCache<'_, T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            decode: self.decode.clone(),
        }
    }
}
impl<'a, T> SourceCache<'a, T> {
    pub fn load(&self, header: MessageHeader, bytes: &[u8]) -> Result<(), BoxedLogError> {
        let value = (self.decode)(bytes)?;
        let mut data = self.data.lock().unwrap();
        if data.messages.iter().any(|m| m.header == header) {
            return Err("duplicate forwarded-source header".into());
        }
        if data.messages.len() >= data.capacity {
            return Err("forwarded-source cache capacity exceeded".into());
        }
        data.publisher
            .publish(value)
            .map_err(|e| -> BoxedLogError { format!("{e:?}").into() })?;
        data.publisher.flush(header.published_at);
        data.subscriber.update();
        let message = data
            .subscriber
            .input()
            .pop()
            .ok_or("source cache did not receive its message")?;
        data.messages.push(message);
        Ok(())
    }
    pub fn decode_forwarded<U: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        bytes: &[u8],
    ) -> Result<task::ForwardedMessage<'a, U, T>, BoxedLogError> {
        task::ForwardedMessage::deserialize_with_ctx(bytes, &*self.data.lock().unwrap())
    }
}

/// Each input has its own hydration publisher, including multiple inputs of one
/// callback on the same logical channel. Declare before storage allocation.
pub struct ReplayInputPlan<T> {
    publisher: PublisherKey<T>,
}
impl<T> ReplayInputPlan<T> {
    pub fn declare(
        plan: &mut ChannelPlan<T>,
        subscriber: &SubscriberKey<T>,
    ) -> Result<Self, StorageError> {
        let publisher = plan.publisher(1);
        plan.restrict_subscriber_sources(subscriber, std::slice::from_ref(&publisher))?;
        Ok(Self { publisher })
    }
    pub fn bind<'a>(
        self,
        bindings: &EndpointBindings<'a, T>,
    ) -> Result<ReplaySource<'a>, task::EndpointError>
    where
        T: Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        Ok(ReplaySource::native(
            bindings.take_publisher(&self.publisher)?,
        ))
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
            bindings.take_publisher(&self.publisher)?,
            decode,
        ))
    }
}
pub(crate) type Port = (String, usize);
pub(crate) type CacheLoader<'a> =
    Box<dyn FnMut(MessageHeader, &[u8]) -> Result<(), BoxedLogError> + Send + 'a>;
/// Bindings use callback names and callback port ordinals, not channel indices.
#[derive(Default)]
pub struct ReplayBindings<'a> {
    pub(crate) inputs: BTreeMap<Port, ReplaySource<'a>>,
    pub(crate) outputs: BTreeMap<Port, PortCapture>,
    pub(crate) caches: BTreeMap<String, CacheLoader<'a>>,
}
impl<'a> ReplayBindings<'a> {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add_input(
        &mut self,
        callback: impl Into<String>,
        ordinal: usize,
        source: ReplaySource<'a>,
    ) -> Result<(), ReplayError> {
        let key = (callback.into(), ordinal);
        if self.inputs.contains_key(&key) {
            return Err(ReplayError::Setup(format!(
                "duplicate input binding {key:?}"
            )));
        }
        self.inputs.insert(key, source);
        Ok(())
    }
    pub fn add_output(
        &mut self,
        callback: impl Into<String>,
        ordinal: usize,
        capture: PortCapture,
    ) -> Result<(), ReplayError> {
        let key = (callback.into(), ordinal);
        if self.outputs.contains_key(&key) {
            return Err(ReplayError::Setup(format!(
                "duplicate output binding {key:?}"
            )));
        }
        self.outputs.insert(key, capture);
        Ok(())
    }
    /// Optional typed source cache for contextual forwarded-message decoders.
    /// Logged values preload at construction; unlogged reproduced outputs load
    /// after their producer executes. Cache storage must be planned by the caller.
    pub fn add_cache(
        &mut self,
        channel: impl Into<String>,
        load: impl FnMut(MessageHeader, &[u8]) -> Result<(), BoxedLogError> + Send + 'a,
    ) -> Result<(), ReplayError> {
        let channel = channel.into();
        if self.caches.contains_key(&channel) {
            return Err(ReplayError::Setup(format!(
                "duplicate source cache '{channel}'"
            )));
        }
        self.caches.insert(channel, Box::new(load));
        Ok(())
    }
}
