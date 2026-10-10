//! Lifetime-preserving serialization capabilities for planned named captures.
use super::{BuildError, NamedBindings, NamedPlan};
use crate::{
    Subscriber, SubscriberKey, SubscriberPolicy, loggable::Loggable, message::MessageHeader,
};
use std::{collections::BTreeSet, marker::PhantomData};

pub type CaptureError = Box<dyn std::error::Error + Send + Sync>;
pub type CaptureVisitor<'a> = dyn FnMut(MessageHeader, &[u8]) -> Result<(), CaptureError> + 'a;

/// An already-bound serialized subscriber; payloads never pass through Any.
pub trait SerializedCapture: Send {
    fn channel(&self) -> &str;
    fn drain(&mut self, visit: &mut CaptureVisitor<'_>) -> Result<(), CaptureError>;
    /// Cumulative writer drops, reader drops and transport receive errors.
    fn loss_counts(&self) -> (usize, usize, usize);
}

pub struct NativeCapture<'a, T> {
    subscriber: Subscriber<'a, T>,
    scratch: Vec<u8>,
}
impl<'a, T> NativeCapture<'a, T> {
    pub fn new(subscriber: Subscriber<'a, T>) -> Self {
        Self {
            subscriber,
            scratch: Vec::new(),
        }
    }
}
impl<T: Loggable + Send + Sync> SerializedCapture for NativeCapture<'_, T> {
    fn channel(&self) -> &str {
        self.subscriber.channel_name()
    }
    fn loss_counts(&self) -> (usize, usize, usize) {
        (
            self.subscriber.writer_drops(),
            self.subscriber.reader_drops(),
            0,
        )
    }
    fn drain(&mut self, visit: &mut CaptureVisitor<'_>) -> Result<(), CaptureError> {
        self.subscriber.update();
        let messages: Vec<_> = self.subscriber.input().drain().collect();
        let (writer, reader, _) = self.loss_counts();
        if writer != 0 || reader != 0 {
            return Err(format!(
                "capture '{}' overflowed (writer {writer}, reader {reader})",
                self.channel()
            )
            .into());
        }
        for message in messages {
            self.scratch.clear();
            message.message.serialize(&mut self.scratch)?;
            visit(message.header, &self.scratch)?;
        }
        Ok(())
    }
}

pub(super) trait CaptureDeclaration {
    fn declare_output(
        &self,
        plan: &mut NamedPlan,
        port: &super::exact::PlannedPort,
    ) -> Result<Box<dyn super::exact::ExactFactory>, BuildError>;
    fn declare(
        &self,
        plan: &mut NamedPlan,
        channel: &str,
        capacity: usize,
    ) -> Result<Box<dyn CaptureFactory>, BuildError>;
}
pub(super) trait CaptureFactory {
    fn bind<'a>(
        self: Box<Self>,
        bindings: &NamedBindings<'a>,
    ) -> Result<Box<dyn SerializedCapture + 'a>, BuildError>;
}
struct NativeDeclaration<T>(PhantomData<fn() -> T>);
struct NativeFactory<T> {
    channel: String,
    subscriber: SubscriberKey<T>,
}
impl<T: Loggable + Send + Sync + 'static> CaptureDeclaration for NativeDeclaration<T> {
    fn declare_output(
        &self,
        plan: &mut NamedPlan,
        port: &super::exact::PlannedPort,
    ) -> Result<Box<dyn super::exact::ExactFactory>, BuildError> {
        Ok(Box::new(NativeOutputFactory {
            channel: port.channel.clone(),
            publisher: plan.native::<T>(&port.channel)?.publisher_key(port.index),
            capacity: port.capacity,
        }))
    }
    fn declare(
        &self,
        plan: &mut NamedPlan,
        channel: &str,
        capacity: usize,
    ) -> Result<Box<dyn CaptureFactory>, BuildError> {
        let subscriber = plan.native::<T>(channel)?.subscriber_with_policy(
            capacity,
            SubscriberPolicy {
                trigger: false,
                keep_across_runs: true,
            },
        );
        Ok(Box::new(NativeFactory {
            channel: channel.into(),
            subscriber,
        }))
    }
}
struct NativeOutputFactory<T> {
    channel: String,
    publisher: crate::PublisherKey<T>,
    capacity: usize,
}
impl<T: Loggable + Send + Sync + 'static> super::exact::ExactFactory for NativeOutputFactory<T> {
    fn bind<'a>(
        self: Box<Self>,
        bindings: &NamedBindings<'a>,
    ) -> Result<super::exact::ExactPort<'a>, BuildError> {
        Ok(super::exact::ExactPort::Output(
            bindings
                .native::<T>(&self.channel)?
                .configure_publisher(&self.publisher, |publisher| {
                    super::port_capture::PortCapture::native(publisher, self.capacity)
                })?,
        ))
    }
}
impl<T: Loggable + Send + Sync + 'static> CaptureFactory for NativeFactory<T> {
    fn bind<'a>(
        self: Box<Self>,
        bindings: &NamedBindings<'a>,
    ) -> Result<Box<dyn SerializedCapture + 'a>, BuildError> {
        Ok(Box::new(NativeCapture::new(
            bindings
                .native::<T>(&self.channel)?
                .take_subscriber(&self.subscriber)?,
        )))
    }
}

/// All subscribers are declared before storage allocation and bound afterwards.
pub struct NamedCapturePlan {
    factories: Vec<Box<dyn CaptureFactory>>,
    channels: Vec<String>,
}
impl NamedCapturePlan {
    pub fn channels(&self) -> &[String] {
        &self.channels
    }
    pub fn bind<'a>(
        self,
        bindings: &NamedBindings<'a>,
    ) -> Result<Vec<Box<dyn SerializedCapture + 'a>>, BuildError> {
        self.factories
            .into_iter()
            .map(|factory| factory.bind(bindings))
            .collect()
    }
}
impl NamedPlan {
    /// Register a custom codec or a manually declared native loggable channel.
    pub fn register_loggable_native<T: Loggable + Send + Sync + 'static>(
        &mut self,
        channel: &str,
    ) -> Result<(), BuildError> {
        if !self.channels.contains_key(channel) {
            return Err(BuildError(format!("unknown channel '{channel}'")));
        }
        self.native::<T>(channel)?;
        self.captures
            .entry(channel.into())
            .or_insert_with(|| Box::new(NativeDeclaration::<T>(PhantomData)));
        Ok(())
    }
    pub fn loggable_channels(&self) -> impl Iterator<Item = &str> {
        self.captures.keys().map(String::as_str)
    }
    /// Call after registering tasks, before allocating their storage.
    /// Exclusions refer to resolved channel names; unmatched exclusions are harmless.
    pub fn declare_captures(
        &mut self,
        capacity: usize,
        excluded: &BTreeSet<String>,
    ) -> Result<NamedCapturePlan, BuildError> {
        if capacity == 0 {
            return Err(BuildError("capture capacity must be nonzero".into()));
        }
        let declarations = std::mem::take(&mut self.captures);
        let result = (|| {
            let mut factories = Vec::new();
            let mut channels = Vec::new();
            for (channel, declaration) in &declarations {
                if !excluded.contains(channel) {
                    factories.push(declaration.declare(self, channel, capacity)?);
                    channels.push(channel.clone());
                }
            }
            Ok(NamedCapturePlan {
                factories,
                channels,
            })
        })();
        self.captures = declarations;
        result
    }
}

/// Stable inherent/trait method resolution probes concrete macro payload types.
pub struct CaptureProbe<T>(PhantomData<fn() -> T>);
impl<T> Default for CaptureProbe<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}
pub trait MaybeCapture {
    fn register_native(&self, _plan: &mut NamedPlan, _channel: &str) -> Result<(), BuildError> {
        Ok(())
    }
    #[cfg(feature = "iceoryx2")]
    fn register_ipc(&self, _plan: &mut NamedPlan, _channel: &str) -> Result<(), BuildError> {
        Ok(())
    }
}
impl<T> MaybeCapture for CaptureProbe<T> {}
impl<T: Loggable + Send + Sync + 'static> CaptureProbe<T> {
    pub fn register_native(&self, plan: &mut NamedPlan, channel: &str) -> Result<(), BuildError> {
        plan.register_loggable_native::<T>(channel)
    }
}

#[cfg(feature = "iceoryx2")]
mod ipc {
    use super::*;
    use crate::iox2::{Iox2Subscriber, Iox2SubscriberKey};
    use iceoryx2::prelude::ZeroCopySend;
    use std::fmt::Debug;
    pub struct IpcCapture<T: Debug + ZeroCopySend + Send + Sync + 'static>(pub Iox2Subscriber<T>);
    impl<T: Loggable + Debug + ZeroCopySend + Send + Sync + 'static> SerializedCapture
        for IpcCapture<T>
    {
        fn channel(&self) -> &str {
            self.0.channel_name()
        }
        fn loss_counts(&self) -> (usize, usize, usize) {
            (0, 0, self.0.receive_errors())
        }
        fn drain(&mut self, visit: &mut CaptureVisitor<'_>) -> Result<(), CaptureError> {
            self.0.update();
            if self.0.receive_errors() != 0 {
                return Err("IPC capture receive error".into());
            }
            let mut result = Ok(());
            self.0.inspect_messages(|_, message| {
                if result.is_ok() {
                    result = (|| {
                        let mut body = Vec::new();
                        message.message.serialize(&mut body)?;
                        visit(message.header, &body)
                    })();
                }
            });
            result
        }
    }
    struct IpcDeclaration<T>(PhantomData<fn() -> T>);
    struct IpcFactory<T> {
        channel: String,
        subscriber: Iox2SubscriberKey<T>,
    }
    impl<T: Loggable + Debug + ZeroCopySend + Send + Sync + 'static> CaptureDeclaration
        for IpcDeclaration<T>
    {
        fn declare_output(
            &self,
            plan: &mut NamedPlan,
            port: &super::super::exact::PlannedPort,
        ) -> Result<Box<dyn super::super::exact::ExactFactory>, BuildError> {
            Ok(Box::new(IpcOutputFactory {
                channel: port.channel.clone(),
                publisher: plan.ipc::<T>(&port.channel)?.publisher_key(port.index),
                capacity: port.capacity,
            }))
        }
        fn declare(
            &self,
            plan: &mut NamedPlan,
            channel: &str,
            capacity: usize,
        ) -> Result<Box<dyn CaptureFactory>, BuildError> {
            let subscriber = plan.ipc::<T>(channel)?.subscriber(capacity);
            Ok(Box::new(IpcFactory {
                channel: channel.into(),
                subscriber,
            }))
        }
    }
    struct IpcOutputFactory<T> {
        channel: String,
        publisher: crate::iox2::Iox2PublisherKey<T>,
        capacity: usize,
    }
    impl<T: Loggable + Debug + ZeroCopySend + Send + Sync + 'static>
        super::super::exact::ExactFactory for IpcOutputFactory<T>
    {
        fn bind<'a>(
            self: Box<Self>,
            bindings: &NamedBindings<'a>,
        ) -> Result<super::super::exact::ExactPort<'a>, BuildError> {
            Ok(super::super::exact::ExactPort::Output(
                bindings.ipc::<T>(&self.channel)?.configure_publisher(
                    &self.publisher,
                    |publisher| {
                        super::super::port_capture::PortCapture::ipc(publisher, self.capacity)
                    },
                )?,
            ))
        }
    }
    impl<T: Loggable + Debug + ZeroCopySend + Send + Sync + 'static> CaptureFactory for IpcFactory<T> {
        fn bind<'a>(
            self: Box<Self>,
            bindings: &NamedBindings<'a>,
        ) -> Result<Box<dyn SerializedCapture + 'a>, BuildError> {
            Ok(Box::new(IpcCapture(
                bindings
                    .ipc::<T>(&self.channel)?
                    .take_subscriber(&self.subscriber)?,
            )))
        }
    }
    impl NamedPlan {
        pub fn register_loggable_ipc<T: Loggable + Debug + ZeroCopySend + Send + Sync + 'static>(
            &mut self,
            channel: &str,
        ) -> Result<(), BuildError> {
            if !self.channels.contains_key(channel) {
                return Err(BuildError(format!("unknown channel '{channel}'")));
            }
            self.ipc::<T>(channel)?;
            self.captures
                .entry(channel.into())
                .or_insert_with(|| Box::new(IpcDeclaration::<T>(PhantomData)));
            Ok(())
        }
    }
    impl<T: Loggable + Debug + ZeroCopySend + Send + Sync + 'static> CaptureProbe<T> {
        pub fn register_ipc(&self, plan: &mut NamedPlan, channel: &str) -> Result<(), BuildError> {
            plan.register_loggable_ipc::<T>(channel)
        }
    }
}
#[cfg(feature = "iceoryx2")]
pub use ipc::IpcCapture;
