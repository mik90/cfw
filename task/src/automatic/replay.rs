//! Context-free decoding capabilities and preallocated named replay publishers.
use super::{BuildError, NamedBindings, NamedPlan};
use crate::{PublisherKey, loggable::Loggable, message::MessageHeader};
use std::{collections::BTreeSet, marker::PhantomData};

pub type ReplayError = Box<dyn std::error::Error + Send + Sync>;
type Inject<'a> = Box<dyn FnMut(MessageHeader, &[u8]) -> Result<(), ReplayError> + Send + 'a>;

/// A decoded publication capability retaining its graph-storage lifetime.
pub struct SerializedSource<'a> {
    channel: String,
    payload_type: &'static str,
    inject: Inject<'a>,
}
impl SerializedSource<'_> {
    pub fn channel(&self) -> &str {
        &self.channel
    }
    pub fn payload_type(&self) -> &'static str {
        self.payload_type
    }
    pub fn inject(&mut self, header: MessageHeader, bytes: &[u8]) -> Result<(), ReplayError> {
        (self.inject)(header, bytes)
    }
}

pub(super) trait SourceDeclaration {
    fn declare(
        &self,
        plan: &mut NamedPlan,
        channel: &str,
        capacity: usize,
    ) -> Result<Box<dyn SourceFactory>, BuildError>;
}
pub(super) trait SourceFactory {
    fn bind<'a>(
        self: Box<Self>,
        bindings: &NamedBindings<'a>,
    ) -> Result<SerializedSource<'a>, BuildError>;
}
struct NativeDeclaration<T>(PhantomData<fn() -> T>);
struct NativeFactory<T> {
    channel: String,
    publisher: PublisherKey<T>,
}
impl<T: Send + Sync + 'static> SourceDeclaration for NativeDeclaration<T>
where
    for<'ctx> T: Loggable<Context<'ctx> = ()>,
{
    fn declare(
        &self,
        plan: &mut NamedPlan,
        channel: &str,
        capacity: usize,
    ) -> Result<Box<dyn SourceFactory>, BuildError> {
        let publisher = plan.native::<T>(channel)?.publisher(capacity);
        Ok(Box::new(NativeFactory {
            channel: channel.into(),
            publisher,
        }))
    }
}
impl<T: Send + Sync + 'static> SourceFactory for NativeFactory<T>
where
    for<'ctx> T: Loggable<Context<'ctx> = ()>,
{
    fn bind<'a>(
        self: Box<Self>,
        bindings: &NamedBindings<'a>,
    ) -> Result<SerializedSource<'a>, BuildError> {
        let mut publisher = bindings
            .native::<T>(&self.channel)?
            .take_publisher(&self.publisher)?;
        Ok(SerializedSource {
            channel: self.channel,
            payload_type: std::any::type_name::<T>(),
            inject: Box::new(move |header, bytes| {
                publisher
                    .publish_with_header(header, T::deserialize(bytes)?)
                    .map_err(|e| -> ReplayError {
                        format!("replay publication failed: {e:?}").into()
                    })
            }),
        })
    }
}

pub struct NamedReplayPlan {
    factories: Vec<Box<dyn SourceFactory>>,
    channels: Vec<String>,
}
impl NamedReplayPlan {
    pub fn channels(&self) -> &[String] {
        &self.channels
    }
    pub fn bind<'a>(
        self,
        bindings: &NamedBindings<'a>,
    ) -> Result<Vec<SerializedSource<'a>>, BuildError> {
        self.factories
            .into_iter()
            .map(|factory| factory.bind(bindings))
            .collect()
    }
}
impl NamedPlan {
    /// Enable decoding for a manually declared native channel. This does not
    /// declare a task publisher or require any Clone/Default payload bound.
    pub fn register_replay_native<T>(&mut self, channel: &str) -> Result<(), BuildError>
    where
        T: Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        if !self.channels.contains_key(channel) {
            return Err(BuildError(format!("unknown channel '{channel}'")));
        }
        self.native::<T>(channel)?;
        self.replay_sources
            .entry(channel.into())
            .or_insert_with(|| Box::new(NativeDeclaration::<T>(PhantomData)));
        Ok(())
    }
    pub fn replayable_channels(&self) -> impl Iterator<Item = &str> {
        self.replay_sources.keys().map(String::as_str)
    }
    /// Select channels explicitly (usually from a log reader), then reserve
    /// injection publishers before allocation. Existing task publishers remain.
    pub fn declare_replay_sources(
        &mut self,
        channels: &BTreeSet<String>,
        capacity: usize,
    ) -> Result<NamedReplayPlan, BuildError> {
        if capacity == 0 || u32::try_from(capacity).is_err() {
            return Err(BuildError(
                "replay publisher capacity must be in 1..=u32::MAX".into(),
            ));
        }
        for channel in channels {
            if channel == crate::recording::EXECUTION_LOG_CHANNEL
                || channel == crate::recording::EXECUTION_EVENT_CHANNEL
            {
                return Err(BuildError(format!(
                    "reserved replay source channel '{channel}'"
                )));
            }
            if !self.replay_sources.contains_key(channel) {
                return Err(BuildError(format!(
                    "channel '{channel}': no context-free replay decoder; register its type, exclude it, or provide an explicit replay source"
                )));
            }
        }
        let declarations = std::mem::take(&mut self.replay_sources);
        let result = (|| {
            let factories = channels
                .iter()
                .map(|channel| declarations[channel].declare(self, channel, capacity))
                .collect::<Result<_, _>>()?;
            Ok(NamedReplayPlan {
                factories,
                channels: channels.iter().cloned().collect(),
            })
        })();
        self.replay_sources = declarations;
        result
    }
}

/// Used on concrete macro payload types to discover context-free decoding.
pub struct DecodeProbe<T>(PhantomData<fn() -> T>);
impl<T> Default for DecodeProbe<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}
pub trait MaybeDecode {
    fn register_native(&self, _plan: &mut NamedPlan, _channel: &str) -> Result<(), BuildError> {
        Ok(())
    }
    #[cfg(feature = "iceoryx2")]
    fn register_ipc(&self, _plan: &mut NamedPlan, _channel: &str) -> Result<(), BuildError> {
        Ok(())
    }
}
impl<T> MaybeDecode for DecodeProbe<T> {}
impl<T: Send + Sync + 'static> DecodeProbe<T>
where
    for<'ctx> T: Loggable<Context<'ctx> = ()>,
{
    pub fn register_native(&self, plan: &mut NamedPlan, channel: &str) -> Result<(), BuildError> {
        plan.register_replay_native::<T>(channel)
    }
}

#[cfg(feature = "iceoryx2")]
mod ipc {
    use super::*;
    use crate::iox2::{Iox2Notification, Iox2PublisherKey};
    use iceoryx2::prelude::ZeroCopySend;
    use std::fmt::Debug;
    struct IpcDeclaration<T>(PhantomData<fn() -> T>);
    struct IpcFactory<T> {
        channel: String,
        publisher: Iox2PublisherKey<T>,
    }
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> SourceDeclaration for IpcDeclaration<T>
    where
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        fn declare(
            &self,
            plan: &mut NamedPlan,
            channel: &str,
            capacity: usize,
        ) -> Result<Box<dyn SourceFactory>, BuildError> {
            let publisher = plan
                .ipc::<T>(channel)?
                .publisher_with_notification(capacity, Iox2Notification::Silent);
            Ok(Box::new(IpcFactory {
                channel: channel.into(),
                publisher,
            }))
        }
    }
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> SourceFactory for IpcFactory<T>
    where
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        fn bind<'a>(
            self: Box<Self>,
            bindings: &NamedBindings<'a>,
        ) -> Result<SerializedSource<'a>, BuildError> {
            let publisher = bindings
                .ipc::<T>(&self.channel)?
                .take_publisher(&self.publisher)?;
            Ok(SerializedSource {
                channel: self.channel,
                payload_type: std::any::type_name::<T>(),
                inject: Box::new(move |header, bytes| {
                    publisher
                        .publish_with_header(header, T::deserialize(bytes)?)
                        .map_err(|e| -> ReplayError {
                            format!("IPC replay publication failed: {e:?}").into()
                        })
                }),
            })
        }
    }
    impl NamedPlan {
        pub fn register_replay_ipc<T>(&mut self, channel: &str) -> Result<(), BuildError>
        where
            T: Debug + ZeroCopySend + Send + Sync + 'static,
            for<'ctx> T: Loggable<Context<'ctx> = ()>,
        {
            if !self.channels.contains_key(channel) {
                return Err(BuildError(format!("unknown channel '{channel}'")));
            }
            self.ipc::<T>(channel)?;
            self.replay_sources
                .entry(channel.into())
                .or_insert_with(|| Box::new(IpcDeclaration::<T>(PhantomData)));
            Ok(())
        }
    }
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> DecodeProbe<T>
    where
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        pub fn register_ipc(&self, plan: &mut NamedPlan, channel: &str) -> Result<(), BuildError> {
            plan.register_replay_ipc::<T>(channel)
        }
    }
}
