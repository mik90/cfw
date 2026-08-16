// Each log task subscribes to a subset of the loggable channels (one
// subscriber slot per channel), drains each subscriber via the matching
// `SerializerFn` from a `ChannelRegistry`, and writes (header, body) pairs to
// a `LogFileWriter` shared with the other log tasks. IO/serialization errors
// are published on the task's own diagnostics channel (see
// `log_task_diagnostics_channel`) as `LogError` messages — the executor
// remains unaware of the logger task; downstream subscribers (e.g. a
// `LogDiagnosticsTask`) decide what to do.

use std::path::Path;
use std::sync::Mutex;

use task::callback::{Callback, PortMut, Run};
use task::context::Context;
use task::execution_log::{self, EXECUTION_LOG_DESCRIPTOR_ARTIFACT};
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::loggable::Loggable;
use task::output::Output;
use task::pub_sub::{CallbackNodeName, ChannelName};
use task::publisher::{Publisher, PublisherConfig};
use task::time::FrameworkTime;

use crate::log_file::{BoxedLogError, LogFileWriter};

/// Name of the `index`-th `LogTask` callback node. Also the base for its
/// diagnostics channel name — see `log_task_diagnostics_channel`.
pub fn log_task_name(index: usize) -> CallbackNodeName {
    format!("LogTask[{index}]")
}

/// Channel the `index`-th `LogTask` publishes its diagnostics on. Named after
/// the logger node so a `LogDiagnosticsTask` can subscribe to one channel per
/// log task.
pub fn log_task_diagnostics_channel(index: usize) -> ChannelName {
    format!("{}_diagnostics", log_task_name(index))
}

/// A single failure observed by a `LogTask` while serializing/writing a
/// channel's message. Published on the task's diagnostics channel (see
/// `log_task_diagnostics_channel`) so a downstream diagnostics task can react
/// (panic, print, count, …).
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LogError {
    pub channel: task::pub_sub::ChannelName,
    pub message: String,
    pub at: FrameworkTime,
}

impl Default for LogError {
    fn default() -> Self {
        LogError {
            channel: String::new(),
            message: String::new(),
            at: FrameworkTime::INVALID,
        }
    }
}

/// One channel's serializer closure (looked up in a `ChannelRegistry` by
/// `value_type_id`) plus a per-channel scratch buffer. `scratch` is per-logger
/// so future per-channel parallelization doesn't need a lock.
pub struct ChannelLogger {
    pub channel_name: task::pub_sub::ChannelName,
    pub serialize: task::channel_registry::SerializerFn,
    pub scratch: Vec<u8>,
}

impl ChannelLogger {
    pub(crate) fn new(
        channel_name: task::pub_sub::ChannelName,
        serialize: task::channel_registry::SerializerFn,
    ) -> Self {
        ChannelLogger {
            channel_name,
            serialize,
            scratch: Vec::new(),
        }
    }

    /// Drain `sub`, serialize each message, write each (header, body) pair to
    /// `writer`. Errors are returned for the caller to publish on the
    /// diagnostics channel — `scratch` is reused across messages.
    fn drain_and_log(
        &mut self,
        sub: &mut dyn GenericSubscriber,
        writer: &mut dyn LogFileWriter,
    ) -> Result<(), BoxedLogError> {
        (self.serialize)(
            sub,
            &mut self.scratch,
            &mut |header: &task::message::MessageHeader,
                  body: &[u8]|
             -> Result<(), BoxedLogError> {
                writer.store_message(&self.channel_name, header, body)
            },
        )
        .map_err(|e| -> BoxedLogError { Box::new(e) })?;
        Ok(())
    }

    /// Drain `sub`, serialize each message, and append each `(header, body)`
    /// pair to `out`. Uses the exact same serializer semantics as
    /// [`Self::drain_and_log`]; the sink collects into memory instead of
    /// writing to a `LogFileWriter`. Used by the replay executor to compare
    /// actual outputs against logged ones.
    pub(crate) fn drain_to_vec(
        &mut self,
        sub: &mut dyn GenericSubscriber,
        out: &mut Vec<(task::message::MessageHeader, Vec<u8>)>,
    ) -> Result<(), BoxedLogError> {
        (self.serialize)(
            sub,
            &mut self.scratch,
            &mut |header: &task::message::MessageHeader,
                  body: &[u8]|
             -> Result<(), BoxedLogError> {
                out.push((*header, body.to_vec()));
                Ok(())
            },
        )
        .map_err(|e| -> BoxedLogError { Box::new(e) })?;
        Ok(())
    }

    /// The channel name this logger drains.
    pub(crate) fn channel_name(&self) -> &str {
        &self.channel_name
    }
}

/// Per-`LogTask` shared error buffer. Rack of `LogError`s collected during a
/// single `run_generic` invocation, drained into the diagnostics publisher
/// and reset before the next run. Mutex so future per-channel
/// parallelization doesn't need restructuring.
#[derive(Default)]
struct ErrorBuffer {
    errors: Mutex<Vec<LogError>>,
}

impl ErrorBuffer {
    fn push(&self, channel: task::pub_sub::ChannelName, message: String, at: FrameworkTime) {
        self.errors
            .lock()
            .expect("error buffer lock poisoned")
            .push(LogError {
                channel,
                message,
                at,
            });
    }

    fn drain(&self) -> Vec<LogError> {
        std::mem::take(
            self.errors
                .lock()
                .expect("error buffer lock poisoned")
                .as_mut(),
        )
    }

    fn is_empty(&self) -> bool {
        self.errors
            .lock()
            .expect("error buffer lock poisoned")
            .is_empty()
    }
}

pub struct ContinuousLogTask {
    writer: Box<dyn LogFileWriter>,
    diagnostics_channel: ChannelName,
    channel_loggers: Vec<ChannelLogger>,
    subscribers: Vec<Box<dyn GenericSubscriber>>,
    diagnostics_publisher: Publisher<LogError>,
    error_buffer: ErrorBuffer,
    execution_log_descriptor: Option<execution_log::ExecutionLogDescriptor>,
}

impl std::fmt::Debug for ContinuousLogTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogTask")
            .field("diagnostics_channel", &self.diagnostics_channel)
            .field("channel_loggers_count", &self.channel_loggers.len())
            .finish_non_exhaustive()
    }
}

impl ContinuousLogTask {
    /// Construct a `LogTask` writing to `writer` and publishing diagnostics on
    /// `diagnostics_channel`. The writer is typically a
    /// `SharedLogFileWriter` clone shared with the other log tasks.
    pub(crate) fn new(
        writer: Box<dyn LogFileWriter>,
        diagnostics_channel: ChannelName,
        channel_loggers: Vec<ChannelLogger>,
        subscribers: Vec<Box<dyn GenericSubscriber>>,
        execution_log_descriptor: Option<execution_log::ExecutionLogDescriptor>,
    ) -> Self {
        let diagnostics_publisher = Publisher::<LogError>::new(PublisherConfig {
            capacity: 1,
            channel_name: diagnostics_channel.clone(),
        });
        ContinuousLogTask {
            writer,
            diagnostics_channel,
            channel_loggers,
            subscribers,
            diagnostics_publisher,
            error_buffer: ErrorBuffer::default(),
            execution_log_descriptor,
        }
    }

    /// Package `e` into a `LogError` and append to the per-run buffer.
    fn record_error(
        &self,
        channel: task::pub_sub::ChannelName,
        e: BoxedLogError,
        at: FrameworkTime,
    ) {
        self.error_buffer.push(channel, e.to_string(), at);
    }

    fn flush(&mut self) -> Result<(), BoxedLogError> {
        self.writer.flush()
    }
}

// Open a writer backed by `JsonLogFileWriter<BufWriter<File>>`. Feature-gated
// on `serde` since `JsonLogFileWriter` is only built with that feature.
// Panics on failure to create/truncate the file — a logger that can't open
// its destination has nothing useful to do at runtime.
#[cfg(feature = "serde")]
pub(crate) fn open_writer(path: &Path) -> Box<dyn LogFileWriter> {
    use std::fs::File;
    use std::io::BufWriter;

    use crate::log_file_json::JsonLogFileWriter;

    let file = File::create(path).unwrap_or_else(|e| {
        panic!(
            "LogTask: failed to open '{}' for writing: {e}",
            path.display()
        )
    });
    let buf = BufWriter::new(file);
    Box::new(JsonLogFileWriter::new(buf))
}

#[cfg(not(feature = "serde"))]
pub(crate) fn open_writer(_path: &Path) -> Box<dyn LogFileWriter> {
    panic!("LogTask::new requires the 'serde' feature for the default JSON writer");
}

impl Callback for ContinuousLogTask {
    fn run(&mut self, ctx: &Context) -> Run {
        // Write execution log descriptor as artifact on first run
        if let Some(descriptor) = self.execution_log_descriptor.take() {
            let mut scratch = Vec::new();
            if let Err(e) = Loggable::serialize(&descriptor, &mut scratch) {
                self.record_error(
                    EXECUTION_LOG_DESCRIPTOR_ARTIFACT.to_owned(),
                    e.into(),
                    ctx.now,
                );
            } else {
                if let Err(e) = self
                    .writer
                    .as_mut()
                    .write_artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &scratch)
                {
                    self.record_error(EXECUTION_LOG_DESCRIPTOR_ARTIFACT.to_owned(), e, ctx.now);
                }
            }
        }

        let mut channel_loggers = std::mem::take(&mut self.channel_loggers);
        let mut subscribers = std::mem::take(&mut self.subscribers);
        for (sub, logger) in subscribers.iter_mut().zip(channel_loggers.iter_mut()) {
            if let Err(e) = logger.drain_and_log(sub.as_mut(), self.writer.as_mut()) {
                self.record_error(logger.channel_name.clone(), e, ctx.now);
            }
        }
        self.channel_loggers = channel_loggers;
        self.subscribers = subscribers;

        if let Err(e) = self.flush() {
            self.record_error("<writer>".to_string(), e, ctx.now);
        }

        if !self.error_buffer.is_empty() {
            for err in self.error_buffer.drain() {
                let mut output = Output::<LogError>::new_default(&mut self.diagnostics_publisher);
                *output = err;
                output.send();
            }
        }

        Run::new(1)
    }

    fn for_each_subscriber<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {
        for s in &self.subscribers {
            f(s.as_ref());
        }
    }
    fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
        f(&self.diagnostics_publisher);
    }
    fn for_each_subscriber_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericSubscriber)) {
        for s in self.subscribers.iter_mut() {
            f(s.as_mut());
        }
    }
    fn for_each_publisher_mut<'a>(&'a mut self, f: &mut dyn FnMut(&'a mut dyn GenericPublisher)) {
        f(&mut self.diagnostics_publisher);
    }
    fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
        for s in self.subscribers.iter_mut() {
            f(PortMut::Subscriber(s.as_mut()));
        }
        f(PortMut::Publisher(&mut self.diagnostics_publisher));
    }
}

// Drop flushes the writer but discards errors — by now the diagnostics
// publisher is gone, so there's no one to tell.
impl Drop for ContinuousLogTask {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
