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

#[cfg(test)]
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

    #[derive(serde::Serialize, serde::Deserialize)]
    struct MyMessage {
        pub my_string: String,
        pub my_bool: bool,
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
