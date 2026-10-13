//! Lifetime-preserving serialization capabilities for planned captures.
use crate::{BuildError, GraphBindings, GraphPlan};
use std::{collections::BTreeSet, marker::PhantomData};
use task::{Subscriber, SubscriberPolicy, loggable::Loggable, message::MessageHeader};

pub type CaptureError = Box<dyn std::error::Error + Send + Sync>;
pub type CaptureVisitor<'a> = dyn FnMut(MessageHeader, &[u8]) -> Result<(), CaptureError> + 'a;

#[derive(Debug, Clone, Copy)]
pub enum CaptureFailureKind {
    Overflow,
    Serialization,
    Write,
    Receive,
}
#[derive(Debug)]
pub struct CaptureFailure {
    pub kind: CaptureFailureKind,
    pub header: Option<MessageHeader>,
    pub error: String,
}
impl std::fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}
impl std::error::Error for CaptureFailure {}
fn failure(
    kind: CaptureFailureKind,
    header: Option<MessageHeader>,
    error: impl ToString,
) -> CaptureError {
    Box::new(CaptureFailure {
        kind,
        header,
        error: error.to_string(),
    })
}

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
        let messages: Vec<_> = self.subscriber.input_forwardable().drain().collect();
        let (writer, reader, _) = self.loss_counts();
        if writer != 0 || reader != 0 {
            return Err(failure(
                CaptureFailureKind::Overflow,
                None,
                format!(
                    "capture '{}' overflowed (writer {writer}, reader {reader})",
                    self.channel()
                ),
            ));
        }
        for message in messages {
            self.scratch.clear();
            message
                .message
                .serialize(&mut self.scratch)
                .map_err(|e| failure(CaptureFailureKind::Serialization, Some(message.header), e))?;
            visit(message.header, &self.scratch)
                .map_err(|e| failure(CaptureFailureKind::Write, Some(message.header), e))?;
        }
        Ok(())
    }
}

pub(crate) type CaptureBinding = Box<
    dyn for<'a> FnOnce(&GraphBindings<'a>) -> Result<Box<dyn SerializedCapture + 'a>, BuildError>,
>;

/// A publisher observer attachment borrowing endpoints only while it is installed.
pub type OutputBinding = Box<
    dyn for<'a> FnOnce(&GraphBindings<'a>) -> Result<super::port_capture::PortCapture, BuildError>,
>;

#[derive(Clone, Copy)]
pub(crate) struct CaptureDeclaration {
    pub declare_output:
        fn(&mut GraphPlan, &super::ports::PlannedPort) -> Result<OutputBinding, BuildError>,
    pub declare: fn(&mut GraphPlan, &str, usize) -> Result<CaptureBinding, BuildError>,
}

fn native_output<T: Loggable + Send + Sync + 'static>(
    plan: &mut GraphPlan,
    port: &super::ports::PlannedPort,
) -> Result<OutputBinding, BuildError> {
    let publisher = plan.native::<T>(&port.channel)?.publisher_key(port.index);
    let channel = port.channel.clone();
    let capacity = port.capacity;
    Ok(Box::new(move |bindings| {
        Ok(bindings
            .native::<T>(&channel)?
            .configure_publisher(&publisher, |publisher| {
                super::port_capture::PortCapture::native(publisher, capacity)
            })?)
    }))
}
fn native<T: Loggable + Send + Sync + 'static>(
    plan: &mut GraphPlan,
    channel: &str,
    capacity: usize,
) -> Result<CaptureBinding, BuildError> {
    let subscriber = plan.native::<T>(channel)?.subscriber_with_policy(
        capacity,
        SubscriberPolicy {
            trigger: false,
            keep_across_runs: true,
        },
    );
    let channel = channel.to_owned();
    Ok(Box::new(move |bindings| {
        Ok(Box::new(NativeCapture::new(
            bindings
                .native::<T>(&channel)?
                .take_subscriber(&subscriber)?,
        )))
    }))
}

/// All subscribers are declared before storage allocation and bound afterwards.
pub struct CapturePlan {
    bindings: Vec<CaptureBinding>,
    channels: Vec<String>,
}
impl CapturePlan {
    pub fn channels(&self) -> &[String] {
        &self.channels
    }
    pub fn bind<'a>(
        self,
        bindings: &GraphBindings<'a>,
    ) -> Result<Vec<Box<dyn SerializedCapture + 'a>>, BuildError> {
        self.bindings
            .into_iter()
            .map(|bind| bind(bindings))
            .collect()
    }
}
impl GraphPlan {
    /// Register a custom codec or a manually declared native loggable channel.
    pub fn register_loggable_native<T: Loggable + Send + Sync + 'static>(
        &mut self,
        channel: &str,
    ) -> Result<(), BuildError> {
        if !self.channels.contains_key(channel) {
            return Err(BuildError(format!("unknown channel '{channel}'")));
        }
        self.native::<T>(channel)?;
        self.channels
            .get_mut(channel)
            .unwrap()
            .capture
            .get_or_insert(CaptureDeclaration {
                declare: native::<T>,
                declare_output: native_output::<T>,
            });
        Ok(())
    }
    pub fn loggable_channels(&self) -> impl Iterator<Item = &str> {
        self.channels
            .iter()
            .filter(|(_, channel)| channel.capture.is_some())
            .map(|(name, _)| name.as_str())
    }
    /// Call after registering tasks, before allocating their storage.
    /// Exclusions refer to resolved channel names; unmatched exclusions are harmless.
    pub fn declare_captures(
        &mut self,
        capacity: usize,
        excluded: &BTreeSet<String>,
    ) -> Result<CapturePlan, BuildError> {
        if capacity == 0 {
            return Err(BuildError("capture capacity must be nonzero".into()));
        }
        let channels: Vec<_> = self
            .loggable_channels()
            .filter(|name| !excluded.contains(*name))
            .map(str::to_owned)
            .collect();
        let bindings = channels
            .iter()
            .map(|channel| {
                let declare = self.channels[channel].capture.unwrap().declare;
                declare(self, channel, capacity)
            })
            .collect::<Result<_, _>>()?;
        Ok(CapturePlan { bindings, channels })
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
    fn register_native(&self, _plan: &mut GraphPlan, _channel: &str) -> Result<(), BuildError> {
        Ok(())
    }
    #[cfg(feature = "iceoryx2")]
    fn register_ipc(&self, _plan: &mut GraphPlan, _channel: &str) -> Result<(), BuildError> {
        Ok(())
    }
}
impl<T> MaybeCapture for CaptureProbe<T> {}
impl<T: Loggable + Send + Sync + 'static> CaptureProbe<T> {
    pub fn register_native(&self, plan: &mut GraphPlan, channel: &str) -> Result<(), BuildError> {
        plan.register_loggable_native::<T>(channel)
    }
}

#[cfg(feature = "iceoryx2")]
mod ipc {
    use super::*;
    use iceoryx2::prelude::ZeroCopySend;
    use std::fmt::Debug;
    use task::iox2::Iox2Subscriber;
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
                return Err(failure(
                    CaptureFailureKind::Receive,
                    None,
                    "IPC capture receive error",
                ));
            }
            let mut result = Ok(());
            self.0.inspect_messages(|_, message| {
                if result.is_ok() {
                    result = (|| {
                        let mut body = Vec::new();
                        message.message.serialize(&mut body).map_err(|e| {
                            failure(CaptureFailureKind::Serialization, Some(message.header), e)
                        })?;
                        visit(message.header, &body).map_err(|e| {
                            failure(CaptureFailureKind::Write, Some(message.header), e)
                        })
                    })();
                }
            });
            result
        }
    }
    fn declare_output<T: Loggable + Debug + ZeroCopySend + Send + Sync + 'static>(
        plan: &mut GraphPlan,
        port: &super::super::ports::PlannedPort,
    ) -> Result<OutputBinding, BuildError> {
        let publisher = plan.ipc::<T>(&port.channel)?.publisher_key(port.index);
        let channel = port.channel.clone();
        let capacity = port.capacity;
        Ok(Box::new(move |bindings| {
            Ok(bindings
                .ipc::<T>(&channel)?
                .configure_publisher(&publisher, |publisher| {
                    super::super::port_capture::PortCapture::ipc(publisher, capacity)
                })?)
        }))
    }
    fn declare<T: Loggable + Debug + ZeroCopySend + Send + Sync + 'static>(
        plan: &mut GraphPlan,
        channel: &str,
        capacity: usize,
    ) -> Result<CaptureBinding, BuildError> {
        let subscriber = plan.ipc::<T>(channel)?.subscriber(capacity);
        let channel = channel.to_owned();
        Ok(Box::new(move |bindings| {
            Ok(Box::new(IpcCapture(
                bindings.ipc::<T>(&channel)?.take_subscriber(&subscriber)?,
            )))
        }))
    }
    impl GraphPlan {
        pub fn register_loggable_ipc<T: Loggable + Debug + ZeroCopySend + Send + Sync + 'static>(
            &mut self,
            channel: &str,
        ) -> Result<(), BuildError> {
            if !self.channels.contains_key(channel) {
                return Err(BuildError(format!("unknown channel '{channel}'")));
            }
            self.ipc::<T>(channel)?;
            self.channels
                .get_mut(channel)
                .unwrap()
                .capture
                .get_or_insert(CaptureDeclaration {
                    declare: declare::<T>,
                    declare_output: declare_output::<T>,
                });
            Ok(())
        }
    }
    impl<T: Loggable + Debug + ZeroCopySend + Send + Sync + 'static> CaptureProbe<T> {
        pub fn register_ipc(&self, plan: &mut GraphPlan, channel: &str) -> Result<(), BuildError> {
            plan.register_loggable_ipc::<T>(channel)
        }
    }
}
#[cfg(feature = "iceoryx2")]
pub use ipc::IpcCapture;
