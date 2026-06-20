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
mod tests {}
