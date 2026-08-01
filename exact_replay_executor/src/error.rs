//! Structured error types for the exact replay executor.

use std::fmt;

use task::execution_log::Direction;
use task::pub_sub::{CallbackNodeName, ChannelName};
use task::time::FrameworkTime;

/// Errors that can occur during exact replay.
#[derive(Debug, Clone)]
pub enum ReplayError {
    /// Execution log entries were dropped in the original run — replay cannot
    /// be deterministic without a complete log.
    DroppedExecutionLogEntries { count: usize },
    /// A channel referenced in the execution log descriptor was not found in
    /// the ChannelRegistry.
    UnregisteredChannel { channel: ChannelName, node: String },
    /// A deserializer for the given channel's type was not found in the
    /// ChannelRegistry.
    UnregisteredDeserializer { channel: ChannelName, node: String },
    /// A serializer for the given channel's type was not found in the
    /// ChannelRegistry (needed for output capture).
    UnregisteredOutputCapture { channel: ChannelName, node: String },
    /// Deserialization of a logged message failed.
    DeserializationFailed {
        channel: ChannelName,
        details: String,
    },
    /// The execution log descriptor could not be parsed or was missing.
    MissingOrInvalidDescriptor(String),
    /// An execution log entry references a callback node index that is out of
    /// range.
    InvalidCallbackNodeIndex { index: usize, node_count: usize },
    /// A subscriber ordinal in the execution log descriptor is out of range
    /// for the callback node.
    InvalidSubscriberOrdinal {
        node: String,
        ordinal: u16,
        subscriber_count: usize,
    },
    /// A publisher ordinal in the execution log descriptor is out of range
    /// for the callback node.
    InvalidPublisherOrdinal {
        node: String,
        ordinal: u16,
        publisher_count: usize,
    },
    /// A callback node panicked during replay.
    CallbackPanic { node: String },
    /// A forwarded-message channel was encountered during replay, which is not
    /// supported in the current implementation.
    UnsupportedForwardedMessage { channel: ChannelName, node: String },
    /// Generic output mismatch during comparison.
    OutputMismatch {
        node: String,
        channel: ChannelName,
        details: String,
    },
    /// An execution from the log references a node index that has no
    /// descriptor entry and is not an infrastructure node.
    DescriptorlessApplicationNode { index: usize, node_name: String },
    /// A message reference in the execution log could not be reproduced: the
    /// channel was logged but its ordinary-log payload is missing, or the
    /// channel was not logged and no in-graph producer exists to reproduce it.
    UnreproducibleMessage {
        channel: ChannelName,
        header_time: FrameworkTime,
        direction: Direction,
        node: CallbackNodeName,
        reason: String,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplayError::DroppedExecutionLogEntries { count } => {
                write!(
                    f,
                    "execution log dropped {count} entries — replay cannot be deterministic"
                )
            }
            ReplayError::UnregisteredChannel { channel, node } => {
                write!(
                    f,
                    "channel '{channel}' (node '{node}') not registered in ChannelRegistry"
                )
            }
            ReplayError::UnregisteredDeserializer { channel, node } => {
                write!(f, "no deserializer for channel '{channel}' (node '{node}')")
            }
            ReplayError::UnregisteredOutputCapture { channel, node } => {
                write!(
                    f,
                    "no output-capture function for channel '{channel}' (node '{node}')"
                )
            }
            ReplayError::DeserializationFailed { channel, details } => {
                write!(
                    f,
                    "deserialization failed for channel '{channel}': {details}"
                )
            }
            ReplayError::MissingOrInvalidDescriptor(msg) => {
                write!(f, "missing or invalid execution log descriptor: {msg}")
            }
            ReplayError::InvalidCallbackNodeIndex { index, node_count } => {
                write!(
                    f,
                    "execution log references callback node index {index}, but only {node_count} nodes exist"
                )
            }
            ReplayError::InvalidSubscriberOrdinal {
                node,
                ordinal,
                subscriber_count,
            } => {
                write!(
                    f,
                    "node '{node}' subscriber ordinal {ordinal} out of range (have {subscriber_count})"
                )
            }
            ReplayError::InvalidPublisherOrdinal {
                node,
                ordinal,
                publisher_count,
            } => {
                write!(
                    f,
                    "node '{node}' publisher ordinal {ordinal} out of range (have {publisher_count})"
                )
            }
            ReplayError::CallbackPanic { node } => {
                write!(f, "callback node '{node}' panicked during replay")
            }
            ReplayError::UnsupportedForwardedMessage { channel, node } => {
                write!(
                    f,
                    "forwarded-message channel '{channel}' (node '{node}') is not supported in exact replay"
                )
            }
            ReplayError::OutputMismatch {
                node,
                channel,
                details,
            } => {
                write!(
                    f,
                    "output mismatch on node '{node}' channel '{channel}': {details}"
                )
            }
            ReplayError::DescriptorlessApplicationNode { index, node_name } => {
                write!(
                    f,
                    "execution references node index {index} ('{node_name}') which has no descriptor entry \
                     and is not an infrastructure node"
                )
            }
            ReplayError::UnreproducibleMessage {
                channel,
                header_time,
                direction,
                node,
                reason,
            } => {
                write!(
                    f,
                    "cannot reproduce {direction:?} message on channel '{channel}' at {header_time} \
                     (node '{node}'): {reason}"
                )
            }
        }
    }
}

impl std::error::Error for ReplayError {}

/// Wrapper error for the executor, bundling per-node panics and per-step
/// replay errors.
#[derive(Debug)]
pub struct ExactReplayExecutorError {
    pub panicked_thread_indices: Vec<usize>,
    pub replay_errors: Vec<ReplayError>,
}

impl fmt::Display for ExactReplayExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.replay_errors.is_empty() {
            write!(f, "replay errors: ")?;
            for (i, err) in self.replay_errors.iter().enumerate() {
                if i > 0 {
                    write!(f, "; ")?;
                }
                write!(f, "{err}")?;
            }
        }
        if !self.panicked_thread_indices.is_empty() {
            if !self.replay_errors.is_empty() {
                write!(f, "; ")?;
            }
            write!(f, "threads panicked: {:?}", self.panicked_thread_indices)?;
        }
        Ok(())
    }
}

impl std::error::Error for ExactReplayExecutorError {}
