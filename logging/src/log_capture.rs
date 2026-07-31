//! Output-capture infrastructure for the exact replay executor.
//!
//! [`PublisherCapture`] binds a typed subscriber to a publisher once via
//! `build_matching_subscriber` + `connect_to_subscriber`, then on each
//! execution the caller flushes the publisher with the replay timestamp,
//! and [`PublisherCapture::drain_to_vec`] serializes the captured messages through the
//! same `SerializerFn` that the `LogTask` uses.
//!
//! This places output serialization entirely in the `logging` crate — the
//! exact replay executor never duplicates serialization logic.

use std::error::Error;

use task::channel_registry::SerializerFn;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::message::MessageHeader;
use task::subscriber::SubscriberConfig;

use crate::log_file::BoxedLogError;
use crate::log_task::ChannelLogger;

/// Error type for `PublisherCapture::connect_from`.
#[derive(Debug)]
pub enum CaptureConnectError {
    /// The publisher's `build_matching_subscriber` returned `None`.
    NoMatchingSubscriber(String),
    /// `connect_to_subscriber` returned a type mismatch.
    TypeMismatch(String),
}

impl std::fmt::Display for CaptureConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureConnectError::NoMatchingSubscriber(ch) => {
                write!(
                    f,
                    "publisher on channel '{ch}' does not support introspection"
                )
            }
            CaptureConnectError::TypeMismatch(ch) => {
                write!(
                    f,
                    "type mismatch connecting capture subscriber to channel '{ch}'"
                )
            }
        }
    }
}

impl Error for CaptureConnectError {}

/// A bound capture subscriber + logger pair. Created once per publisher
/// channel during replay setup, then reused across executions.
pub struct PublisherCapture {
    /// The logger holds the `SerializerFn` and per-call scratch buffer.
    pub logger: ChannelLogger,
    /// The subscriber that receives flushed values from the publisher.
    /// The replay executor connects a publisher to this subscriber once,
    /// then flushes the publisher into it each execution.
    pub subscriber: Box<dyn GenericSubscriber>,
}

impl PublisherCapture {
    /// Build a `PublisherCapture` by asking `publisher` to build a matching
    /// subscriber, then connecting `publisher` to it. Uses the publisher's
    /// own `config.capacity` for the capture subscriber's queue depth.
    ///
    /// After this call the publisher's arena capacity has been increased to
    /// account for the capture subscriber's footprint. The caller **must**
    /// call `publisher.allocate_arena()` after this to materialise the slots
    /// — this is safe only when the publisher's arena has not yet been
    /// allocated (i.e. the node is a fresh replay graph, not a live one).
    ///
    /// Returns a structured error instead of panicking.
    pub fn connect_from(
        channel_name: String,
        serialize: SerializerFn,
        publisher: &mut dyn GenericPublisher,
    ) -> Result<Self, CaptureConnectError> {
        let capacity = publisher.config().capacity;
        let sub_config = SubscriberConfig {
            is_optional: true,
            capacity,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: channel_name.clone(),
        };
        let mut subscriber = publisher
            .build_matching_subscriber(sub_config)
            .ok_or_else(|| CaptureConnectError::NoMatchingSubscriber(channel_name.clone()))?;
        publisher
            .connect_to_subscriber(subscriber.as_mut())
            .map_err(|_| CaptureConnectError::TypeMismatch(channel_name.clone()))?;
        Ok(PublisherCapture {
            logger: ChannelLogger::new(channel_name, serialize),
            subscriber,
        })
    }

    /// Drain the capture subscriber through the logger, collecting
    /// `(header, serialized_body)` pairs into `out`. Drains write→read
    /// before serialization, and clears the subscriber after so it is
    /// ready for the next execution.
    pub fn drain_to_vec(
        &mut self,
        out: &mut Vec<(MessageHeader, Vec<u8>)>,
    ) -> Result<(), BoxedLogError> {
        self.subscriber.drain_writer_to_reader();
        self.logger.drain_to_vec(self.subscriber.as_mut(), out)?;
        Ok(())
    }
}

/// Drain a capture subscriber through a logger, collecting `(header,
/// serialized_body)` pairs into `out`. Provided as a free function so
/// callers can use it without borrowing the entire `PublisherCapture`.
pub fn drain_capture(
    logger: &mut ChannelLogger,
    sub: &mut dyn GenericSubscriber,
    out: &mut Vec<(MessageHeader, Vec<u8>)>,
) -> Result<(), BoxedLogError> {
    sub.drain_writer_to_reader();
    logger.drain_to_vec(sub, out)?;
    Ok(())
}
