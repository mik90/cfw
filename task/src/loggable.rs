use std::error::Error;
use std::fmt::Display;
use std::io::{self, Write};

use crate::message::{Message, MessageHeader};

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
    MissingForwardedMessage,
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
            DeserializeError::MissingForwardedMessage => {
                write!(f, "MissingForwardedMessage")
            }
            #[cfg(feature = "serde")]
            DeserializeError::SerdeJson(e) => write!(f, "SerdeJson: {e}"),
        }
    }
}

impl Error for DeserializeError {}

pub trait Loggable: Sized {
    type Context<'a>;

    fn serialize(&self, w: &mut dyn Write) -> Result<(), SerializeError>;
    fn deserialize_with_ctx(bytes: &[u8], ctx: Self::Context<'_>)
    -> Result<Self, DeserializeError>;

    fn deserialize(bytes: &[u8]) -> Result<Self, DeserializeError>
    where
        Self: Loggable<Context<'static> = ()>,
    {
        Self::deserialize_with_ctx(bytes, ())
    }
}

/// Blanket impl for serde types
#[cfg(feature = "serde")]
impl<T> Loggable for T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    type Context<'a> = ();

    fn serialize(&self, w: &mut dyn Write) -> Result<(), SerializeError> {
        serde_json::to_writer(w, self).map_err(SerializeError::SerdeJson)
    }

    fn deserialize_with_ctx(bytes: &[u8], _ctx: ()) -> Result<Self, DeserializeError> {
        serde_json::from_slice(bytes).map_err(DeserializeError::SerdeJson)
    }
}

/// Allows lookup of a message from the log. Assumes that the log is scoped to a given channel.
pub trait MessageLog<T> {
    fn lookup(&self, header: &MessageHeader) -> Option<&Message<T>>;
}

/// Implement loggability for ForwardedMessage.
/// Specific to serde, sadly, since the handling is a bit specific to the serialization backend.
/// (De)serialization is asymmetric. Serialization only serializes the header. Deserialization performs a lookup on the log
/// given the serialized header. This means that the logging system avoids logging messages twice, but it does entail some channel
/// interdepencies when logging so that lookup won't fail.
#[cfg(feature = "serde")]
impl<UserData, ForwardedData> Loggable
    for crate::forwarded_message::ForwardedMessage<UserData, ForwardedData>
where
    UserData: serde::Serialize + serde::de::DeserializeOwned,
    ForwardedData: Clone + 'static,
{
    type Context<'a> = &'a dyn MessageLog<ForwardedData>;

    fn serialize(&self, w: &mut dyn Write) -> Result<(), SerializeError> {
        #[derive(serde::Serialize)]
        struct Envelope<'a, U> {
            message: &'a U,
            forwarded_message_header: &'a MessageHeader,
        }
        let env = Envelope {
            message: self.get_message(),
            forwarded_message_header: &self.get_forwarded_message().header,
        };
        serde_json::to_writer(w, &env).map_err(SerializeError::SerdeJson)
    }

    fn deserialize_with_ctx(
        bytes: &[u8],
        ctx: &dyn MessageLog<ForwardedData>,
    ) -> Result<Self, DeserializeError> {
        #[derive(serde::Deserialize)]
        struct Envelope<U> {
            message: U,
            forwarded_message_header: MessageHeader,
        }
        let env: Envelope<UserData> =
            serde_json::from_slice(bytes).map_err(DeserializeError::SerdeJson)?;

        let forwarded = ctx
            .lookup(&env.forwarded_message_header)
            .ok_or(DeserializeError::MissingForwardedMessage)?;

        Ok(
            crate::forwarded_message::ForwardedMessage::new_boxed_forward(
                env.message,
                Box::new(forwarded.clone()),
            ),
        )
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::Loggable;
    use crate::{
        forwarded_message::ForwardedMessage, message::Message, message::MessageHeader,
        time::FrameworkTime,
    };

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

    #[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
    struct MyMessage {
        pub my_string: String,
        pub my_bool: bool,
    }

    #[test]
    fn test_forwarded_message_serializes_header_only() {
        let forwarded_message = ForwardedMessage::new_boxed_forward(
            false,
            Box::new(Message {
                header: MessageHeader::new(FrameworkTime::from_nanoseconds(2)),
                message: MyMessage {
                    my_bool: true,
                    my_string: "Hello".to_owned(),
                },
            }),
        );
        let mut buf = Vec::new();
        forwarded_message.serialize(&mut buf).unwrap();

        let json: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert!(json.get("message").is_some());
        assert!(json.get("forwarded_message_header").is_some());
        assert!(json.get("forwarded_message").is_none());
    }

    #[test]
    fn test_forwarded_message_round_trip_with_log() {
        use super::MessageLog;

        let original_msg = Message {
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(2)),
            message: MyMessage {
                my_bool: true,
                my_string: "Hello".to_owned(),
            },
        };

        struct TestLog {
            messages: Vec<Message<MyMessage>>,
        }
        impl MessageLog<MyMessage> for TestLog {
            fn lookup(&self, header: &MessageHeader) -> Option<&Message<MyMessage>> {
                self.messages
                    .iter()
                    .find(|m| m.header.published_at == header.published_at)
            }
        }

        let log = TestLog {
            messages: vec![original_msg.clone()],
        };

        let forwarded_message = ForwardedMessage::new_boxed_forward(
            false,
            Box::new(Message {
                header: MessageHeader::new(FrameworkTime::from_nanoseconds(2)),
                message: MyMessage {
                    my_bool: true,
                    my_string: "Hello".to_owned(),
                },
            }),
        );
        let mut buf = Vec::new();
        forwarded_message.serialize(&mut buf).unwrap();

        let deserialized =
            ForwardedMessage::<bool, MyMessage>::deserialize_with_ctx(&buf, &log).unwrap();
        assert_eq!(*deserialized.get_message(), false);

        let fwd = deserialized.get_forwarded_message();
        assert_eq!(fwd.message.my_string, "Hello");
        assert_eq!(fwd.message.my_bool, true);
        assert_eq!(fwd.header.published_at, FrameworkTime::from_nanoseconds(2));
    }

    #[test]
    fn test_message_logging() {
        let header = MessageHeader::new(FrameworkTime::from_nanoseconds(42));
        let payload = MyMessage {
            my_string: "Hello".to_owned(),
            my_bool: true,
        };

        let message = Message {
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
