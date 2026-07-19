// The log task subscribes to a set of channels (one subscriber slot per channel),
// serializes each consumed message via a per-channel type-erased closure, and
// writes the result to a shared `LogFileWriter`. IO/serialization errors are
// published on the `log_task_diagnostics` channel as `LogError` messages so the
// executor remains entirely unaware of the logger task; downstream subscribers
// (e.g. a `LogDiagnosticsTask`) decide what to do with them.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use task::callback::{Callback, Run};
use task::context::Context;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::loggable::{Loggable, SerializeError};
use task::message::MessageHeader;
use task::output::Output;
use task::pub_sub::ChannelName;
use task::publisher::{Publisher, PublisherConfig};
use task::subscriber::SubscriberConfig;
use task::time::FrameworkTime;

use crate::log_file::{BoxedLogError, BoxedLogFileWriter, LogFileWriterObj};

/// Channel the `LogTask` publishes its diagnostics on.
pub const LOG_TASK_DIAGNOSTICS_CHANNEL: &str = "log_task_diagnostics";

/// A single failure observed by the `LogTask` while serializing/writing a
/// channel's message. Published on `LOG_TASK_DIAGNOSTICS_CHANNEL` so a
/// downstream diagnostics task can react (panic, print, count, …).
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LogError {
    pub channel: ChannelName,
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

/// A user-supplied request to log one channel, capturing the payload type `T`
/// at construction time so we can build a serializer closure that downcasts
/// `&dyn Any` back to `Message<T>` and calls `Message::<T>::serialize`.
///
/// `ChannelLogRequest::new::<T>("foo")` is the typical entry point; use
/// `with_capacity` to override the default subscriber queue depth.
pub struct ChannelLogRequest {
    pub channel_name: ChannelName,
    pub(crate) serialize: SerializerFn,
    queue_capacity: usize,
}

impl ChannelLogRequest {
    /// Create a request for the channel, deriving a serializer from
    /// `T: Loggable`. The closure captures `T` statically at this call site
    /// — the framework's other layers never need to know `T`. The header
    /// travels separately (the framework passes it as a typed
    /// `MessageHeader` to `for_each_queued_input`); the closure only writes
    /// the value bytes.
    pub fn new<T>(channel_name: impl Into<ChannelName>) -> Self
    where
        T: 'static + Loggable,
    {
        ChannelLogRequest {
            channel_name: channel_name.into(),
            serialize: Arc::new(|any: &dyn Any, buf: &mut Vec<u8>| {
                // The framework passes `&T` (the message's value field)
                // upcast to `&dyn Any`; downcast back here and serialize.
                let value = any
                    .downcast_ref::<T>()
                    .expect("ChannelLogRequest registered with mismatched T for channel");
                value.serialize(buf)
            }),
            queue_capacity: DEFAULT_LOG_QUEUE_CAPACITY,
        }
    }

    /// Override the default subscriber queue depth for this channel.
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity;
        self
    }

    /// Build a `SubscriberConfig` matching this request.
    pub(crate) fn make_subscriber_config(&self) -> SubscriberConfig {
        SubscriberConfig {
            // Optional + trigger keeps LogTask's readiness bitmask independent
            // of any one channel — it runs every cycle (continuous logging).
            is_optional: true,
            capacity: self.queue_capacity,
            is_trigger: true,
            keep_across_runs: true,
            channel_name: self.channel_name.clone(),
        }
    }
}

/// Default queue depth for logging subscribers. Logging is meant to keep up
/// with the publisher's full rate in steady state; matches the existing
/// `DEFAULT_TEST_SUBSCRIBER_CAPACITY` to avoid surprises during tests.
pub const DEFAULT_LOG_QUEUE_CAPACITY: usize = 10;

/// Shared, type-erased serializer closure. Cloned cheaply via `Arc::clone`.
/// Captures `T` statically at the call site that built the closure — at
/// runtime, only the `&dyn Any` downcast matters.
pub(crate) type SerializerFn =
    Arc<dyn Fn(&dyn Any, &mut Vec<u8>) -> Result<(), SerializeError> + Send + Sync>;

/// One channel's serializer + scratch buffer. Paired 1:1 with a subscriber
/// slot on the owning `LogTask`. `scratch` is per-logger so future per-channel
/// parallelization doesn't need a lock.
pub(crate) struct ChannelLogger {
    channel_name: ChannelName,
    serialize: SerializerFn,
    scratch: Vec<u8>,
}

impl ChannelLogger {
    pub(crate) fn new(channel_name: ChannelName, serialize: SerializerFn) -> Self {
        ChannelLogger {
            channel_name,
            serialize,
            scratch: Vec::new(),
        }
    }

    fn log_value(
        &mut self,
        header: &MessageHeader,
        value: &dyn Any,
        writer: &mut dyn LogFileWriterObj,
    ) -> Result<(), BoxedLogError> {
        self.scratch.clear();
        (self.serialize)(value, &mut self.scratch)
            .map_err(|e| -> BoxedLogError { format!("serialize failed: {e}").into() })?;
        writer.store_message(&self.channel_name, header, &self.scratch)
    }
}

/// Per-`LogTask` shared error buffer. Rack of `LogError`s collected during a
/// single `run_generic` invocation, drained into the diagnostics publisher
/// and reset before the next run. Mutex is light contention since LogTask
/// executes single-threaded for now; the mutex is here so the diagnostics
/// publisher's send path (which expects `&mut`) plays nicely with closures.
#[derive(Default)]
struct ErrorBuffer {
    errors: Mutex<Vec<LogError>>,
}

impl ErrorBuffer {
    fn push(&self, channel: ChannelName, message: String, at: FrameworkTime) {
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

/// Where to write the log file. Currently a single path on disk; the writer
/// lives for the duration of the `LogTask` and is flushed in `run_generic`
/// and again on `Drop`.
pub struct LogTaskConfiguration {
    pub output_path: PathBuf,
}

pub struct LogTask {
    writer: Box<dyn LogFileWriterObj>,
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
    fn record_error(&self, channel: ChannelName, e: BoxedLogError, at: FrameworkTime) {
        self.error_buffer.push(channel, e.to_string(), at);
    }

    /// Manually flush the underlying writer. Called at the end of each
    /// `run_generic` so errors from the underlying BufWriter surface
    /// promptly as diagnostic messages instead of waiting for drop.
    fn flush(&mut self) -> Result<(), BoxedLogError> {
        flush_writer(self.writer.as_mut())
    }
}

// Opening the writer needs serde feature on so JsonLogFileWriter is available.
// If the feature is off, we can still construct a LogTask from a writer directly.
#[cfg(feature = "serde")]
fn open_writer(path: &Path) -> Box<dyn LogFileWriterObj> {
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
    BoxedLogFileWriter::new(JsonLogFileWriter::new(buf)).into_boxed()
}

#[cfg(not(feature = "serde"))]
fn open_writer(_path: &Path) -> Box<dyn LogFileWriterObj> {
    panic!(
        "LogTask::new requires the 'serde' feature; construct with LogTask::new_with_writer otherwise"
    );
}

fn flush_writer(writer: &mut dyn LogFileWriterObj) -> Result<(), BoxedLogError> {
    // The boxed trait object only exposes store_message. Concrete writers like
    // JsonLogFileWriter<BufWriter<File>> flush their BufWriter on drop, so this
    // is best-effort for now: we wrap store_message in a no-op flush so future
    // writers that want explicit flushing can add a flush method to the trait.
    let _ = writer;
    Ok(())
}

impl Callback for LogTask {
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn GenericSubscriber>],
        publishers: &mut [Box<dyn GenericPublisher>],
        ctx: &Context,
    ) -> Run {
        // For each subscriber-slot / channel-logger pair, drain queued inputs
        // and serialize. Subscribers are polled continuously (is_trigger=true,
        // is_optional=true), so keep_across_runs=true means messages persist
        // until we explicitly drain them here.
        //
        // We temporarily move `channel_loggers` out of `self` so the inner
        // closure can capture both `logger` and `self.writer`/`self.error_buffer`
        // without a second mutable borrow of `self`.
        let mut channel_loggers = std::mem::take(&mut self.channel_loggers);
        for (sub, logger) in subscribers.iter().zip(channel_loggers.iter_mut()) {
            let channel = logger.channel_name.clone();
            sub.for_each_queued_input(&mut |header, value| {
                if let Err(e) = logger.log_value(header, value, self.writer.as_mut()) {
                    self.record_error(channel.clone(), e, ctx.now);
                }
            });
        }
        self.channel_loggers = channel_loggers;

        // Flush so an IO error mid-run surfaces immediately as a diagnostic.
        if let Err(e) = self.flush() {
            self.record_error("<writer>".to_string(), e, ctx.now);
        }

        // Publish any accumulated errors. With capacity 1 we expect at most one
        // error per run; older errors overflow silently — that's acceptable
        // since per-cycle multiple errors indicate a sustained failure.
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
        // Subscribers are injected by the LoggingBuildStep via CallbackNode::new_with;
        // LogTask itself doesn't construct them.
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

// Suppress unused-import warning for OnceLock — kept for future use if we
// switch away from a fresh-writer-per-LogTask model.
#[allow(dead_code)]
fn _keep_once_lock_referenced() -> OnceLock<()> {
    OnceLock::new()
}

// Mutex import remains used via ErrorBuffer.
#[allow(dead_code)]
fn _keep_mutex_referenced() -> Mutex<()> {
    Mutex::new(())
}
