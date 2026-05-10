use crate::time::FrameworkTime;
use std::collections::VecDeque;

use crate::context::Context;
use crate::generic_publisher::GenericPublisher;
use crate::generic_subscriber::GenericSubscriber;
use crate::message::MessageHeader;

// ── Serialization traits ─────────────────────────────────────────────────────

/// Pluggable serialization — no external dependencies required by this trait.
/// Users implement it for their message types with whatever backend they prefer.
/// The caller provides the destination buffer, so no `Vec<u8>` is allocated per call.
///
/// Example (serde_json):
/// ```ignore
/// const TYPE_NAME: &'static str = "MyMsg";
/// fn serialize_to_log(&self, out: &mut Vec<u8>) {
///     serde_json::to_writer(out, self).unwrap();
/// }
/// ```
pub trait LogSerialize {
    /// The Rust type name used to tag serialized bytes in the log.
    const TYPE_NAME: &'static str;
    fn serialize_to_log(&self, out: &mut Vec<u8>);
}

/// Deserializes from an already-materialized byte slice.
/// Use `std::io::Cursor` if your backend requires a `Read` impl.
pub trait LogDeserialize: Sized {
    type Error: std::fmt::Display;
    fn deserialize_from_log(bytes: &[u8]) -> Result<Self, Self::Error>;
}

// ── Log entry types ──────────────────────────────────────────────────────────

/// One entry per message in the subscriber's read buffer at execution time.
#[derive(Clone)]
pub struct InputLogEntry {
    pub header: MessageHeader,
    /// `None` unless `ChannelLogMode::CountAndValues` is configured for this channel.
    pub serialized: Option<Vec<u8>>,
}

/// One entry per output sent by the publisher during this execution.
#[derive(Clone)]
pub struct OutputLogEntry {
    pub header: MessageHeader,
    /// `None` unless `ChannelLogMode::CountAndValues` is configured for this channel.
    pub serialized: Option<Vec<u8>>,
}

/// All inputs logged for one subscriber slot.
#[derive(Clone)]
pub struct SubscriberLog {
    pub entries: Vec<InputLogEntry>,
}

/// All outputs logged for one publisher slot.
#[derive(Clone)]
pub struct PublisherLog {
    pub entries: Vec<OutputLogEntry>,
}

/// One record per callback execution. `subscribers` and `publishers` are indexed
/// to match the order in `ConnectedCallback`.
#[derive(Clone)]
pub struct CallbackExecutionLog {
    pub execution_time: FrameworkTime,
    pub subscribers: Vec<SubscriberLog>,
    pub publishers: Vec<PublisherLog>,
}

// ── LogWriter / LogReader traits ─────────────────────────────────────────────

pub trait LogWriter: Send {
    /// Called once per callback execution. The entry is borrowed — clone if you need to own it.
    fn log_execution(&mut self, entry: &CallbackExecutionLog);
}

pub trait LogReader: Send {
    fn next_execution(&mut self) -> Option<CallbackExecutionLog>;
    fn peek_next_time(&self) -> Option<FrameworkTime>;
}

// ── ExecutionLogger trait ────────────────────────────────────────────────────

pub trait ExecutionLogger: Send {
    /// Called after `drain_subscribers()`, before `run_generic()`.
    fn log_before_run(&mut self, ctx: &Context, subscribers: &[Box<dyn GenericSubscriber>]);

    /// Called after `run_generic()`, before `flush_publishers()`.
    fn log_after_run(&mut self, ctx: &Context, publishers: &[Box<dyn GenericPublisher>]);
}

// ── StandardExecutionLogger ──────────────────────────────────────────────────

/// The closure receives `&dyn std::any::Any` and a `&mut dyn Write` to write into.
type HeaderAndValueSerializerCallback = dyn Fn(&dyn std::any::Any, &mut Vec<u8>) + Send;

/// Per-channel logging mode. Configure independently for each subscriber/publisher slot.
pub enum ChannelLogMode {
    /// Don't record this channel.
    Skip,
    /// Record each message's header (timestamp) only.
    HeaderOnly,
    /// Record each message's header plus serialized value.
    HeaderAndValues(Box<HeaderAndValueSerializerCallback>),
}

/// A ready-to-use `ExecutionLogger` that covers the most common logging needs.
/// Pre-allocates its internal buffer and reuses it each cycle to minimize allocation.
#[allow(dead_code)] // just while these TODOs are here
pub struct StandardExecutionLogger {
    subscriber_modes: Vec<ChannelLogMode>,
    publisher_modes: Vec<ChannelLogMode>,
    log_writer: Box<dyn LogWriter>,
    /// Reused each cycle — `Vec::clear()` retains capacity.
    buffer: CallbackExecutionLog,
    /// Scratch buffer for `LogSerialize` calls — cleared before each use.
    serialize_scratch: Vec<u8>,
}

impl StandardExecutionLogger {
    pub fn builder(log_writer: Box<dyn LogWriter>) -> StandardExecutionLoggerBuilder {
        StandardExecutionLoggerBuilder::new(log_writer)
    }
}

impl ExecutionLogger for StandardExecutionLogger {
    #[allow(unused_variables)] // just while these TODOs are here
    fn log_before_run(&mut self, ctx: &Context, subscribers: &[Box<dyn GenericSubscriber>]) {
        self.buffer.execution_time = ctx.now;

        // Ensure the subscriber log vec has the right number of slots, reusing existing ones.
        while self.buffer.subscribers.len() < subscribers.len() {
            self.buffer.subscribers.push(SubscriberLog {
                entries: Vec::new(),
            });
        }
        self.buffer.subscribers.truncate(subscribers.len());

        for (i, sub) in subscribers.iter().enumerate() {
            let sub_log = &mut self.buffer.subscribers[i];
            sub_log.entries.clear();

            let mode = self
                .subscriber_modes
                .get(i)
                .unwrap_or(&ChannelLogMode::Skip);
            match mode {
                ChannelLogMode::Skip => {}
                ChannelLogMode::HeaderOnly => {
                    todo!("Figure out how to just get the header from std::any::Any")
                    /*
                    sub.for_each_queued_input(&mut |_value| {
                        sub_log.entries.push(InputLogEntry {
                            header: MessageHeader {
                                published_at: header.published_at,
                            },
                            serialized: None,
                        });
                    });
                    */
                }
                ChannelLogMode::HeaderAndValues(serialize_fn) => {
                    todo!("Figure out how to just get the header from std::any::Any")
                    /*
                    sub.for_each_queued_input(&mut |value| {
                        self.serialize_scratch.clear();
                        serialize_fn(value, &mut self.serialize_scratch);
                        sub_log.entries.push(InputLogEntry {
                            header: MessageHeader {
                                published_at: header.published_at,
                            },
                            serialized: Some(self.serialize_scratch.clone()),
                        });
                    });
                    */
                }
            }
        }
    }

    #[allow(unused_variables)] // just while these TODOs are here
    fn log_after_run(&mut self, ctx: &Context, publishers: &[Box<dyn GenericPublisher>]) {
        while self.buffer.publishers.len() < publishers.len() {
            self.buffer.publishers.push(PublisherLog {
                entries: Vec::new(),
            });
        }
        self.buffer.publishers.truncate(publishers.len());

        for (i, pub_) in publishers.iter().enumerate() {
            let pub_log = &mut self.buffer.publishers[i];
            pub_log.entries.clear();

            let mode = self.publisher_modes.get(i).unwrap_or(&ChannelLogMode::Skip);
            match mode {
                ChannelLogMode::Skip => {}
                ChannelLogMode::HeaderOnly => {
                    todo!("Figure out how to just get the header from std::any::Any")
                    /*
                    pub_.for_each_pending_output(ctx.now, &mut |value| {
                        pub_log.entries.push(OutputLogEntry {
                            header: MessageHeader {
                                published_at: header.published_at,
                            },
                            serialized: None,
                        });
                    }); */
                }
                ChannelLogMode::HeaderAndValues(serialize_fn) => {
                    todo!("Figure out how to just get the header from std::any::Any")
                    /*
                    pub_.for_each_pending_output(ctx.now, &mut |value| {
                        self.serialize_scratch.clear();
                        serialize_fn(value, &mut self.serialize_scratch);
                        pub_log.entries.push(OutputLogEntry {
                            serialized: Some(self.serialize_scratch.clone()),
                        });
                    }); */
                }
            }
        }

        self.log_writer.log_execution(&self.buffer);
    }
}

// ── StandardExecutionLoggerBuilder ───────────────────────────────────────────

pub struct StandardExecutionLoggerBuilder {
    subscriber_modes: Vec<ChannelLogMode>,
    publisher_modes: Vec<ChannelLogMode>,
    log_writer: Box<dyn LogWriter>,
}

impl StandardExecutionLoggerBuilder {
    fn new(log_writer: Box<dyn LogWriter>) -> Self {
        StandardExecutionLoggerBuilder {
            subscriber_modes: Vec::new(),
            publisher_modes: Vec::new(),
            log_writer,
        }
    }

    /// Set the logging mode for subscriber at `index`.
    pub fn subscriber(mut self, index: usize, mode: ChannelLogMode) -> Self {
        if self.subscriber_modes.len() <= index {
            self.subscriber_modes
                .resize_with(index + 1, || ChannelLogMode::Skip);
        }
        self.subscriber_modes[index] = mode;
        self
    }

    /// Set the logging mode for publisher at `index`.
    pub fn publisher(mut self, index: usize, mode: ChannelLogMode) -> Self {
        if self.publisher_modes.len() <= index {
            self.publisher_modes
                .resize_with(index + 1, || ChannelLogMode::Skip);
        }
        self.publisher_modes[index] = mode;
        self
    }

    pub fn build(self) -> StandardExecutionLogger {
        let buffer = CallbackExecutionLog {
            execution_time: crate::time::FrameworkTime::from_wall_clock(),
            subscribers: Vec::new(),
            publishers: Vec::new(),
        };
        StandardExecutionLogger {
            subscriber_modes: self.subscriber_modes,
            publisher_modes: self.publisher_modes,
            log_writer: self.log_writer,
            buffer,
            serialize_scratch: Vec::new(),
        }
    }
}

// ── In-memory LogWriter / LogReader for testing ──────────────────────────────

pub struct InMemoryLogWriter {
    entries: VecDeque<CallbackExecutionLog>,
}

impl InMemoryLogWriter {
    pub fn new() -> Self {
        InMemoryLogWriter {
            entries: VecDeque::new(),
        }
    }

    pub fn into_reader(self) -> InMemoryLogReader {
        InMemoryLogReader {
            entries: self.entries,
        }
    }
}

impl Default for InMemoryLogWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl LogWriter for InMemoryLogWriter {
    fn log_execution(&mut self, entry: &CallbackExecutionLog) {
        self.entries.push_back(entry.clone());
    }
}

pub struct InMemoryLogReader {
    entries: VecDeque<CallbackExecutionLog>,
}

impl LogReader for InMemoryLogReader {
    fn next_execution(&mut self) -> Option<CallbackExecutionLog> {
        self.entries.pop_front()
    }

    fn peek_next_time(&self) -> Option<FrameworkTime> {
        self.entries.front().map(|e| e.execution_time)
    }
}
