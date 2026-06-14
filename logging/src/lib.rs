use std::error::Error;
use std::fmt::Display;
use std::io::{self, Write};

#[derive(Debug, Display)]
pub enum SerializeError {
    IoError(io::Error),
}

impl Display for SerializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SerializeError::IoError(i) => {
                write!(f, "IoError: {}", i)?;
            }
        }
        Ok(())
    }
}

impl Error for SerializeError {}

#[derive(Debug)]
pub enum DeserializeError {
    MismatchedSize((usize, usize)), // Actual vs expected
}

impl Display for DeserializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeserializeError::MismatchedSize((actual, expected)) => {
                write!(f, "MismatchedSize: Actual({actual} vs Expected({expected})")?;
            }
        }
        Ok(())
    }
}

impl Error for DeserializeError {}

pub trait Loggable<T: Sized> {
    // What do we want the interface to look like?
    // User should provide buffer.
    // But do we need separate traits for fixed and dynamic sizing? Could just have a Writer

    fn serialize(&self, w: dyn Write) -> Result<(), SerializeError>;

    fn deserialize(bytes: &[u8]) -> Result<T, DeserializeError>;
}

#[cfg(test)]
mod tests {}
