// The log task subscribes to a set of channels (one subscriber slot per channel),
// drains each subscriber via the matching `SerializerFn` from a
// `ChannelRegistry`, and writes (header, body) pairs to a shared `LogFileWriter`.
// IO/serialization errors are published on the `log_task_diagnostics` channel
// as `LogError` messages — the executor remains unaware of the logger task;
// downstream subscribers (e.g. a `LogDiagnosticsTask`) decide what to do.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use task::callback::{Callback, Run};
use task::context::Context;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::output::Output;
use task::publisher::{Publisher, PublisherConfig};
use task::time::FrameworkTime;

use crate::log_file::{BoxedLogError, LogFileWriter};

/// Channel the `LogTask` publishes its diagnostics on.
pub const LOG_TASK_DIAGNOSTICS_CHANNEL: &str = "log_task_diagnostics";

/// A single failure observed by the `LogTask` while serializing/writing a
/// channel's message. Published on `LOG_TASK_DIAGNOSTICS_CHANNEL` so a
/// downstream diagnostics task can react (panic, print, count, …).
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
pub(crate) struct ChannelLogger {
    channel_name: task::pub_sub::ChannelName,
    serialize: task::channel_registry::SerializerFn,
    scratch: Vec<u8>,
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

/// Where to write the log file and how often to run the logger.
/// The writer lives for the duration of the `LogTask` and is flushed in
/// `run_generic` and again on `Drop`.
pub struct LogTaskConfiguration {
    pub output_path: PathBuf,
    pub period: Duration,
}

pub struct LogTask {
    writer: Box<dyn LogFileWriter>,
    channel_loggers: Vec<ChannelLogger>,
    error_buffer: ErrorBuffer,
}

impl std::fmt::Debug for LogTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogTask")
            .field("channel_loggers_count", &self.channel_loggers.len())
            .finish_non_exhaustive()
    }
}

impl LogTask {
    /// Construct a `LogTask` writing to a fresh file at `output_path`.
    /// Panic on failure to create/truncate the file — a logger that can't
    /// open its destination has nothing useful to do at runtime.
    pub(crate) fn new(config: &LogTaskConfiguration, channel_loggers: Vec<ChannelLogger>) -> Self {
        let writer = open_writer(&config.output_path);
        LogTask {
            writer,
            channel_loggers,
            error_buffer: ErrorBuffer::default(),
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
#[cfg(feature = "serde")]
fn open_writer(path: &Path) -> Box<dyn LogFileWriter> {
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
fn open_writer(_path: &Path) -> Box<dyn LogFileWriter> {
    panic!("LogTask::new requires the 'serde' feature for the default JSON writer");
}

impl Callback for LogTask {
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn GenericSubscriber>],
        publishers: &mut [Box<dyn GenericPublisher>],
        ctx: &Context,
    ) -> Run {
        // For each subscriber-slot / channel-logger pair: drain the subscriber,
        // serialize each message's value, write (header, body) to the shared
        // writer. Move `channel_loggers` out of `self` to avoid a
        // simultaneous borrow of `self.channel_loggers` and `self.writer`.
        let mut channel_loggers = std::mem::take(&mut self.channel_loggers);
        for (sub, logger) in subscribers.iter_mut().zip(channel_loggers.iter_mut()) {
            if let Err(e) = logger.drain_and_log(sub.as_mut(), self.writer.as_mut()) {
                self.record_error(logger.channel_name.clone(), e, ctx.now);
            }
        }
        self.channel_loggers = channel_loggers;

        // Flush so an IO error mid-run surfaces immediately as a diagnostic.
        if let Err(e) = self.flush() {
            self.record_error("<writer>".to_string(), e, ctx.now);
        }

        // Publish any accumulated errors. Publisher capacity is 1 — at most
        // one emits per cycle; older errors overflow silently. Sustained
        // per-cycle multiple errors indicate a sustained failure mode that
        // the one-message-per-cycle shape can't capture anyway.
        if !self.error_buffer.is_empty()
            && let Some(diagnostics) = publishers
                .iter_mut()
                .find(|p| p.get_config().channel_name == LOG_TASK_DIAGNOSTICS_CHANNEL)
            && let Some(typed) = diagnostics.as_any().downcast_mut::<Publisher<LogError>>()
        {
            for err in self.error_buffer.drain() {
                let mut output: Output<'_, LogError> = Output::new_default(typed);
                *output = err;
                output.send();
            }
        }

        Run::new(1)
    }

    fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
        // Subscribers are injected by the LoggingBuildStep via
        // `CallbackNode::new_with`; LogTask itself doesn't construct them.
        vec![]
    }

    fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
        vec![Box::new(Publisher::<LogError>::new(PublisherConfig {
            capacity: 1,
            channel_name: LOG_TASK_DIAGNOSTICS_CHANNEL.to_string(),
        }))]
    }
}

// Drop flushes the writer but discards errors — by now the diagnostics
// publisher is gone, so there's no one to tell.
impl Drop for LogTask {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
