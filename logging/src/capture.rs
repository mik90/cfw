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
type Visitor<'a> = dyn FnMut(MessageHeader, &[u8]) -> Result<(), BoxedLogError> + 'a;
trait Drain: Send {
    fn drain(&mut self, visit: &mut Visitor<'_>) -> Result<(), BoxedLogError>;
}
struct Native<'a, T> {
    subscriber: Subscriber<'a, T>,
    scratch: Vec<u8>,
}
impl<T: Loggable + Send + Sync> Drain for Native<'_, T> {
    fn drain(&mut self, visit: &mut Visitor<'_>) -> Result<(), BoxedLogError> {
        self.subscriber.update();
        let messages: Vec<_> = self.subscriber.input().drain().collect();
        if self.subscriber.writer_drops() != 0 || self.subscriber.reader_drops() != 0 {
            return Err(format!(
                "capture '{}' overflowed (writer {}, reader {})",
                self.subscriber.channel_name(),
                self.subscriber.writer_drops(),
                self.subscriber.reader_drops()
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
/// Type-erased capture retaining the storage lifetime, never requiring Clone or Any.
pub struct Capture<'a> {
    channel: String,
    drain: Box<dyn Drain + 'a>,
}
impl<'a> Capture<'a> {
    pub fn native<T: Loggable + Send + Sync + 'a>(subscriber: Subscriber<'a, T>) -> Self {
        Self {
            channel: subscriber.channel_name().into(),
            drain: Box::new(Native {
                subscriber,
                scratch: Vec::new(),
            }),
        }
    }
    pub fn channel(&self) -> &str {
        &self.channel
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
            struct Ipc<T: std::fmt::Debug + iceoryx2::prelude::ZeroCopySend + Send + Sync + 'static>(
                Iox2Subscriber<T>,
            );
            impl<
                T: Loggable
                    + std::fmt::Debug
                    + iceoryx2::prelude::ZeroCopySend
                    + Send
                    + Sync
                    + 'static,
            > Drain for Ipc<T>
            {
                fn drain(&mut self, visit: &mut Visitor<'_>) -> Result<(), BoxedLogError> {
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
            Self {
                channel: subscriber.channel_name().into(),
                drain: Box::new(Ipc(subscriber)),
            }
        }
    }
}
