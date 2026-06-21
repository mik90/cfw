use crate::arena::ArenaReaderPtr;
use crate::message::Message;
use std::ops::Deref;

enum ForwardedMessagePtr<T> {
    Arena(ArenaReaderPtr<T>),
    Owned(Box<T>), // Mainly used for log deserialization
}

impl<T> Deref for ForwardedMessagePtr<T> {
    type Target = T;
    fn deref(&self) -> &T {
        match self {
            ForwardedMessagePtr::Arena(ptr) => ptr,
            ForwardedMessagePtr::Owned(b) => b,
        }
    }
}

/// A single forwarded message.
///
/// If no extra data is required, you can use `()` as the extra data type.
///
/// For multiple, i think i need multiple trait spceializations since we don't have variadics?
/// And then the getters would need to be named more uniquely, or we return a tuple.
pub struct ForwardedMessage<UserData, ForwardedData> {
    pub(crate) message: UserData,
    forwarded_message: ForwardedMessagePtr<Message<ForwardedData>>,
}

impl<UserData, ForwardedData> ForwardedMessage<UserData, ForwardedData> {
    #[cfg(feature = "testing")]
    pub fn new_boxed_forward(
        message: UserData,
        forwarded_message: Box<Message<ForwardedData>>,
    ) -> Self {
        ForwardedMessage {
            message,
            forwarded_message: ForwardedMessagePtr::Owned(forwarded_message),
        }
    }
}

impl<UserData, ForwardedData> ForwardedMessage<UserData, ForwardedData> {
    pub fn get_message(&self) -> &UserData {
        &self.message
    }

    pub fn get_message_mut(&mut self) -> &mut UserData {
        &mut self.message
    }

    pub fn get_forwarded_message(&self) -> &Message<ForwardedData> {
        &self.forwarded_message
    }
}

#[cfg(feature = "serde")]
impl<UserData, ForwardedData> serde::Serialize for ForwardedMessage<UserData, ForwardedData>
where
    UserData: serde::Serialize,
    ForwardedData: serde::Serialize,
{
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ForwardedMessage", 2)?;
        state.serialize_field("message", &self.message)?;
        state.serialize_field("forwarded_message", &*self.forwarded_message)?;
        state.end()
    }
}

#[cfg(feature = "serde")]
impl<'de, UserData, ForwardedData> serde::Deserialize<'de>
    for ForwardedMessage<UserData, ForwardedData>
where
    UserData: serde::Deserialize<'de>,
    ForwardedData: serde::Deserialize<'de>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Raw<U, F> {
            message: U,
            forwarded_message: Message<F>,
        }

        let raw = Raw::<UserData, ForwardedData>::deserialize(deserializer)?;
        Ok(ForwardedMessage {
            message: raw.message,
            forwarded_message: ForwardedMessagePtr::Owned(Box::new(raw.forwarded_message)),
        })
    }
}

impl<UserData: Default, ForwardedData> ForwardedMessage<UserData, ForwardedData> {
    pub fn new_with_forward(forwarded_message: ArenaReaderPtr<Message<ForwardedData>>) -> Self {
        Self {
            message: UserData::default(),
            forwarded_message: ForwardedMessagePtr::Arena(forwarded_message),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::generic_publisher::*;
    use crate::input::*;
    use crate::output::*;
    use crate::publisher::*;
    use crate::subscriber::*;
    use crate::time::FrameworkTime;

    /// More of an integration test, but checks that forwarded pub/sub works
    #[test]
    fn forwarded_pub_sub() {
        let normal_channel = "channel";
        let forwarded_channel = "forwarded_channel";

        // Owner of messages being forwarded in the arena
        let mut normal_publisher = Publisher::<u32>::new(PublisherConfig {
            capacity: 1,
            channel_name: forwarded_channel.into(),
        });

        // Subscribes to message with intention of forwarding it
        let mut forwardable_subscriber = ForwardableSubscriber::<u32>::new(SubscriberConfig {
            is_optional: true,
            capacity: 2,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: normal_channel.into(),
        });

        // Forwards normal message downstream with an additional bool payload
        let mut forwarding_publisher = ForwardingPublisher::<bool, u32>::new(
            PublisherConfig {
                capacity: 1,
                channel_name: forwarded_channel.into(),
            },
            vec![forwarded_channel.into()],
        );

        // Listens to forwarded message
        let mut forwarded_subscriber =
            Subscriber::<ForwardedMessage<bool, u32>>::new(SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: forwarded_channel.into(),
            });

        normal_publisher.add_typed_forwarded_subscriber(&mut forwardable_subscriber);
        normal_publisher.allocate_arena();

        forwarding_publisher.add_typed_subscriber(&mut forwarded_subscriber);
        forwarding_publisher.allocate_arena();
        assert_eq!(
            forwarding_publisher.get_forwarded_channels(),
            vec![forwarded_channel]
        );

        let t_original = FrameworkTime::from_nanoseconds(42);
        let t_forwarding = FrameworkTime::from_nanoseconds(99);
        {
            // Publishes message that'll be forwarded downstream
            let mut output = Output::new_default(&mut normal_publisher);
            *output = 42u32;
            output.send();
            normal_publisher.flush_loaned_values(t_original);
        }
        {
            // Callback that forwards from its subscriber to publisher, adding a bool payload
            forwardable_subscriber.subscriber.drain_writer_to_reader();
            let input = ForwardableOptionalInput::new(&mut forwardable_subscriber);
            assert!(input.value().is_some());
            assert_eq!(*input.value().unwrap(), 42u32);

            let mut fwd_out = ForwardingOutput::new(&mut forwarding_publisher);
            let mut output = input.forward(&mut fwd_out).unwrap();
            *output = true;
            output.send();
            forwarding_publisher.flush_loaned_values(t_forwarding);
        }
        {
            // Callback that reads the forwarded message
            forwarded_subscriber.drain_writer_to_reader();
            let guard = forwarded_subscriber.get_read_buffer();
            let msg = guard.front().unwrap();
            assert_eq!(msg.header.published_at, t_forwarding);
            assert_eq!(*msg.message.get_message(), true);
            let fwd = msg.message.get_forwarded_message();
            assert_eq!(fwd.message, 42u32);
            assert_eq!(fwd.header.published_at, t_original);
        }
    }

    /// Forwards a message without extra data
    #[test]
    fn forwarded_without_extra_data() {
        let normal_channel = "channel";
        let forwarded_channel = "forwarded_channel";

        // Owner of messages being forwarded in the arena
        let mut normal_publisher = Publisher::<u32>::new(PublisherConfig {
            capacity: 1,
            channel_name: forwarded_channel.into(),
        });

        // Subscribes to message with intention of forwarding it
        let mut forwardable_subscriber = ForwardableSubscriber::<u32>::new(SubscriberConfig {
            is_optional: true,
            capacity: 2,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: normal_channel.into(),
        });

        // Forwards normal message downstream without additional payload
        let mut forwarding_publisher = ForwardingPublisher::<(), u32>::new(
            PublisherConfig {
                capacity: 1,
                channel_name: forwarded_channel.into(),
            },
            vec![forwarded_channel.into()],
        );

        // Listens to forwarded message
        let mut forwarded_subscriber =
            Subscriber::<ForwardedMessage<(), u32>>::new(SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: forwarded_channel.into(),
            });

        normal_publisher.add_typed_forwarded_subscriber(&mut forwardable_subscriber);
        normal_publisher.allocate_arena();

        forwarding_publisher.add_typed_subscriber(&mut forwarded_subscriber);
        forwarding_publisher.allocate_arena();
        assert_eq!(
            forwarding_publisher.get_forwarded_channels(),
            vec![forwarded_channel]
        );

        let t_original = FrameworkTime::from_nanoseconds(42);
        let t_forwarding = FrameworkTime::from_nanoseconds(99);
        {
            // Publishes message that'll be forwarded downstream
            let mut output = Output::new_default(&mut normal_publisher);
            *output = 42u32;
            output.send();
            normal_publisher.flush_loaned_values(t_original);
        }
        {
            // Callback that forwards from its subscriber to publisher, adding a bool payload
            forwardable_subscriber.subscriber.drain_writer_to_reader();
            let input = ForwardableOptionalInput::new(&mut forwardable_subscriber);
            assert!(input.value().is_some());
            assert_eq!(*input.value().unwrap(), 42u32);

            let mut fwd_out = ForwardingOutput::new(&mut forwarding_publisher);
            let output = input.forward(&mut fwd_out).unwrap();
            output.send();
            forwarding_publisher.flush_loaned_values(t_forwarding);
        }
        {
            // Callback that reads the forwarded message
            forwarded_subscriber.drain_writer_to_reader();
            let guard = forwarded_subscriber.get_read_buffer();
            let msg = guard.front().unwrap();
            assert_eq!(msg.header.published_at, t_forwarding);
            let fwd = msg.message.get_forwarded_message();
            assert_eq!(fwd.message, 42u32);
            assert_eq!(fwd.header.published_at, t_original);
        }
    }
}
