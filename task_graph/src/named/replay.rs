//! Replay decoding capabilities and preallocated injection endpoints.
use super::{BuildError, GraphBindings, GraphPlan, ports::PlannedPort};
use std::{collections::BTreeSet, marker::PhantomData};
use task::{Publisher, loggable::Loggable, message::MessageHeader};

pub type ReplayError = Box<dyn std::error::Error + Send + Sync>;
type Inject<'a> = Box<dyn FnMut(MessageHeader, &[u8]) -> Result<(), ReplayError> + Send + 'a>;

/// A decoded publication capability retaining its graph-storage lifetime.
pub struct ReplaySource<'a> {
    channel: String,
    payload_type: &'static str,
    inject: Inject<'a>,
}
impl<'a> ReplaySource<'a> {
    pub fn native<T>(publisher: Publisher<'a, T>) -> Self
    where
        T: Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        Self::with_decoder(publisher, T::deserialize)
    }
    pub fn with_decoder<T: Send + Sync + 'a>(
        mut publisher: Publisher<'a, T>,
        mut decode: impl FnMut(&[u8]) -> Result<T, ReplayError> + Send + 'a,
    ) -> Self {
        Self {
            channel: publisher.channel_name().into(),
            payload_type: std::any::type_name::<T>(),
            inject: Box::new(move |header, bytes| {
                publisher
                    .publish_with_header(header, decode(bytes)?)
                    .map_err(|e| format!("replay publication failed: {e:?}").into())
            }),
        }
    }
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
                    .map_err(|e| format!("IPC replay publication failed: {e:?}").into())
            }),
        }
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
                    .map_err(|e| format!("IPC replay input failed: {e:?}").into())
            }),
        }
    }
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

pub type SourceBinding =
    Box<dyn for<'a> FnOnce(&GraphBindings<'a>) -> Result<ReplaySource<'a>, BuildError>>;

#[derive(Clone, Copy)]
pub(super) struct SourceDeclaration {
    pub declare: fn(&mut GraphPlan, &str, usize) -> Result<SourceBinding, BuildError>,
    pub inject_input: fn(&mut GraphPlan, &PlannedPort, usize) -> Result<SourceBinding, BuildError>,
}

fn native<T>(
    plan: &mut GraphPlan,
    channel: &str,
    capacity: usize,
) -> Result<SourceBinding, BuildError>
where
    T: Send + Sync + 'static,
    for<'ctx> T: Loggable<Context<'ctx> = ()>,
{
    let publisher = plan.native::<T>(channel)?.publisher(capacity);
    let channel = channel.to_owned();
    Ok(Box::new(move |bindings| {
        Ok(ReplaySource::native(
            bindings.native::<T>(&channel)?.take_publisher(&publisher)?,
        ))
    }))
}
fn native_input<T>(
    plan: &mut GraphPlan,
    port: &PlannedPort,
    capacity: usize,
) -> Result<SourceBinding, BuildError>
where
    T: Send + Sync + 'static,
    for<'ctx> T: Loggable<Context<'ctx> = ()>,
{
    let channel = plan.native::<T>(&port.channel)?;
    let subscriber = channel.subscriber_key(port.index);
    let publisher = channel.publisher(capacity);
    channel.restrict_subscriber_sources(&subscriber, std::slice::from_ref(&publisher))?;
    let channel = port.channel.clone();
    Ok(Box::new(move |bindings| {
        Ok(ReplaySource::native(
            bindings.native::<T>(&channel)?.take_publisher(&publisher)?,
        ))
    }))
}

pub struct NamedReplayPlan {
    bindings: Vec<SourceBinding>,
    channels: Vec<String>,
}
impl NamedReplayPlan {
    pub fn channels(&self) -> &[String] {
        &self.channels
    }
    pub fn bind<'a>(
        self,
        bindings: &GraphBindings<'a>,
    ) -> Result<Vec<ReplaySource<'a>>, BuildError> {
        self.bindings
            .into_iter()
            .map(|bind| bind(bindings))
            .collect()
    }
}
impl GraphPlan {
    /// Register context-free decoding for an existing native channel.
    pub fn register_replay_native<T>(&mut self, channel: &str) -> Result<(), BuildError>
    where
        T: Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        if !self.channels.contains_key(channel) {
            return Err(BuildError(format!("unknown channel '{channel}'")));
        }
        self.native::<T>(channel)?;
        self.channels
            .get_mut(channel)
            .unwrap()
            .replay
            .get_or_insert(SourceDeclaration {
                declare: native::<T>,
                inject_input: native_input::<T>,
            });
        Ok(())
    }
    pub fn replayable_channels(&self) -> impl Iterator<Item = &str> {
        self.channels
            .iter()
            .filter(|(_, channel)| channel.replay.is_some())
            .map(|(name, _)| name.as_str())
    }
    /// Reserve injection publishers before allocation, retaining task publishers.
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
            if !self
                .channels
                .get(channel)
                .is_some_and(|channel| channel.replay.is_some())
            {
                return Err(BuildError(format!(
                    "channel '{channel}': no context-free replay decoder; register its type, exclude it, or provide an explicit replay source"
                )));
            }
        }
        let bindings = channels
            .iter()
            .map(|channel| {
                let declare = self.channels[channel].replay.unwrap().declare;
                declare(self, channel, capacity)
            })
            .collect::<Result<_, _>>()?;
        Ok(NamedReplayPlan {
            bindings,
            channels: channels.iter().cloned().collect(),
        })
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
    fn register_native(&self, _plan: &mut GraphPlan, _channel: &str) -> Result<(), BuildError> {
        Ok(())
    }
    #[cfg(feature = "iceoryx2")]
    fn register_ipc(&self, _plan: &mut GraphPlan, _channel: &str) -> Result<(), BuildError> {
        Ok(())
    }
}
impl<T> MaybeDecode for DecodeProbe<T> {}
impl<T: Send + Sync + 'static> DecodeProbe<T>
where
    for<'ctx> T: Loggable<Context<'ctx> = ()>,
{
    pub fn register_native(&self, plan: &mut GraphPlan, channel: &str) -> Result<(), BuildError> {
        plan.register_replay_native::<T>(channel)
    }
}

#[cfg(feature = "iceoryx2")]
mod ipc {
    use super::*;
    use iceoryx2::prelude::ZeroCopySend;
    use std::fmt::Debug;
    use task::iox2::Iox2Notification;

    fn declare<T>(
        plan: &mut GraphPlan,
        channel: &str,
        capacity: usize,
    ) -> Result<SourceBinding, BuildError>
    where
        T: Debug + ZeroCopySend + Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        let publisher = plan
            .ipc::<T>(channel)?
            .publisher_with_notification(capacity, Iox2Notification::Silent);
        let channel = channel.to_owned();
        Ok(Box::new(move |bindings| {
            Ok(ReplaySource::ipc(
                bindings.ipc::<T>(&channel)?.take_publisher(&publisher)?,
            ))
        }))
    }
    fn inject_input<T>(
        plan: &mut GraphPlan,
        port: &PlannedPort,
        _capacity: usize,
    ) -> Result<SourceBinding, BuildError>
    where
        T: Debug + ZeroCopySend + Send + Sync + 'static,
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        let subscriber = plan.ipc::<T>(&port.channel)?.subscriber_key(port.index);
        let channel = port.channel.clone();
        Ok(Box::new(move |bindings| {
            Ok(ReplaySource::ipc_input(
                bindings.ipc::<T>(&channel)?.replay_input(&subscriber)?,
            ))
        }))
    }
    impl GraphPlan {
        pub fn register_replay_ipc<T>(&mut self, channel: &str) -> Result<(), BuildError>
        where
            T: Debug + ZeroCopySend + Send + Sync + 'static,
            for<'ctx> T: Loggable<Context<'ctx> = ()>,
        {
            if !self.channels.contains_key(channel) {
                return Err(BuildError(format!("unknown channel '{channel}'")));
            }
            self.ipc::<T>(channel)?;
            self.channels
                .get_mut(channel)
                .unwrap()
                .replay
                .get_or_insert(SourceDeclaration {
                    declare: declare::<T>,
                    inject_input: inject_input::<T>,
                });
            Ok(())
        }
    }
    impl<T: Debug + ZeroCopySend + Send + Sync + 'static> DecodeProbe<T>
    where
        for<'ctx> T: Loggable<Context<'ctx> = ()>,
    {
        pub fn register_ipc(&self, plan: &mut GraphPlan, channel: &str) -> Result<(), BuildError> {
            plan.register_replay_ipc::<T>(channel)
        }
    }
}
