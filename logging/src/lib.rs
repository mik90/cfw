use std::error::Error;
use std::fmt::Display;
use std::io::{self, Write};

#[derive(Debug)]
pub enum SerializeError {
    IoError(io::Error),
    #[cfg(feature = "serde")]
    SerdeJson(serde_json::Error),
}

impl Display for SerializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SerializeError::IoError(i) => write!(f, "IoError: {i}"),
            #[cfg(feature = "serde")]
            SerializeError::SerdeJson(e) => write!(f, "SerdeJson: {e}"),
        }
    }
}

impl Error for SerializeError {}

impl From<io::Error> for SerializeError {
    fn from(e: io::Error) -> Self {
        SerializeError::IoError(e)
    }
}

#[derive(Debug)]
pub enum DeserializeError {
    MismatchedSize {
        actual: usize,
        expected: usize,
    },
    #[cfg(feature = "serde")]
    SerdeJson(serde_json::Error),
}

impl Display for DeserializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeserializeError::MismatchedSize { actual, expected } => {
                write!(
                    f,
                    "MismatchedSize: actual({actual}) vs expected({expected})"
                )
            }
            #[cfg(feature = "serde")]
            DeserializeError::SerdeJson(e) => write!(f, "SerdeJson: {e}"),
        }
    }
}

impl Error for DeserializeError {}

pub trait Loggable: Sized {
    fn serialize(&self, w: &mut dyn Write) -> Result<(), SerializeError>;
    fn deserialize(bytes: &[u8]) -> Result<Self, DeserializeError>;
}

#[cfg(feature = "serde")]
impl<T> Loggable for T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    fn serialize(&self, w: &mut dyn Write) -> Result<(), SerializeError> {
        serde_json::to_writer(w, self).map_err(SerializeError::SerdeJson)
    }

    fn deserialize(bytes: &[u8]) -> Result<Self, DeserializeError> {
        serde_json::from_slice(bytes).map_err(DeserializeError::SerdeJson)
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::Loggable;
    use task::{message::Message, message::MessageHeader, time::FrameworkTime};

    #[test]
    fn test_header_logging() {
        let header = MessageHeader::new(FrameworkTime::from_nanoseconds(42));

        let mut buf = Vec::new();
        header.serialize(&mut buf).unwrap();

        let deserialized = MessageHeader::deserialize(&buf).unwrap();
        assert_eq!(
            deserialized.published_at,
            FrameworkTime::from_nanoseconds(42)
        );
    }

    #[derive(Default, serde::Serialize, serde::Deserialize)]
    struct MyMessage {
        pub my_string: String,
        pub my_bool: bool,
    }

    #[test]
    fn test_forwarded_message_logging() {
        use task::forwarded_message::ForwardedMessage;
        use task::generic_publisher::*;
        use task::input::*;
        use task::output::*;
        use task::publisher::*;
        use task::subscriber::*;

        let normal_channel = "channel";
        let forwarded_channel = "forwarded_channel";

        let mut normal_publisher = Publisher::<MyMessage>::new(PublisherConfig {
            capacity: 1,
            channel_name: forwarded_channel.into(),
        });

        let mut forwardable_subscriber =
            ForwardableSubscriber::<MyMessage>::new(SubscriberConfig {
                is_optional: true,
                capacity: 2,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: normal_channel.into(),
            });

        let mut forwarding_publisher = ForwardingPublisher::<bool, MyMessage>::new(
            PublisherConfig {
                capacity: 1,
                channel_name: forwarded_channel.into(),
            },
            vec![forwarded_channel.into()],
        );

        let mut forwarded_subscriber =
            Subscriber::<ForwardedMessage<bool, MyMessage>>::new(SubscriberConfig {
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

        let t_original = FrameworkTime::from_nanoseconds(42);
        let t_forwarding = FrameworkTime::from_nanoseconds(99);

        {
            let mut output = Output::new_default(&mut normal_publisher);
            *output = MyMessage {
                my_string: "Hello".to_owned(),
                my_bool: true,
            };
            output.send();
            normal_publisher.flush_loaned_values(t_original);
        }
        {
            forwardable_subscriber.subscriber.drain_writer_to_reader();
            let input = ForwardableOptionalInput::new(&mut forwardable_subscriber);
            let mut fwd_out = ForwardingOutput::new(&mut forwarding_publisher);
            let mut output = input.forward(&mut fwd_out).unwrap();
            *output = false;
            output.send();
            forwarding_publisher.flush_loaned_values(t_forwarding);
        }
        {
            forwarded_subscriber.drain_writer_to_reader();
            let guard = forwarded_subscriber.get_read_buffer();
            let msg = guard.front().unwrap();

            let mut buf = Vec::new();
            msg.message.serialize(&mut buf).unwrap();

            let deserialized = ForwardedMessage::<bool, MyMessage>::deserialize(&buf).unwrap();
            assert_eq!(*deserialized.get_message(), false);
            let fwd = deserialized.get_forwarded_message();
            assert_eq!(fwd.message.my_string, "Hello");
            assert_eq!(fwd.message.my_bool, true);
            assert_eq!(fwd.header.published_at, t_original);
        }
    }

    #[test]
    fn test_message_logging() {
        let header = MessageHeader::new(FrameworkTime::from_nanoseconds(42));
        let payload = MyMessage {
            my_string: "Hello".to_owned(),
            my_bool: true,
        };

        let message = task::message::Message {
            header,
            message: payload,
        };

        let mut buf = Vec::new();
        message.serialize(&mut buf).unwrap();

        let deserialized = Message::<MyMessage>::deserialize(&buf).unwrap();
        assert_eq!(
            deserialized.header.published_at,
            FrameworkTime::from_nanoseconds(42)
        );
        assert_eq!(deserialized.message.my_string, "Hello",);
        assert_eq!(deserialized.message.my_bool, true);
    }
}
