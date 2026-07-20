use std::fmt;
use std::sync::{Arc, Mutex};

use task::message::MessageHeader;

/// Error type used by all `LogFileWriter` implementations. Anything that can
/// fail during logging ultimately surfaces as one of these so the writer can
/// be stored behind a single trait object without exposing a concrete
/// associated type.
pub type BoxedLogError = Box<dyn std::error::Error + Send + Sync>;

/// Object-safe trait for writing log entries to a backing store (file,
/// in-memory buffer, …). `store_message` returns `BoxedLogError` so any
/// concrete writer can be wrapped as `Box<dyn LogFileWriter>` and stored
/// uniformly on the `LogTask`.
pub trait LogFileWriter: Send {
    fn store_message(
        &mut self,
        channel_name: &str,
        header: &MessageHeader,
        body: &[u8],
    ) -> Result<(), BoxedLogError>;

    /// Flush any buffered writes to the underlying sink. The default no-op
    /// suits writers that don't buffer; concrete writers like
    /// `JsonLogFileWriter<BufWriter<_>>` should override to flush.
    fn flush(&mut self) -> Result<(), BoxedLogError> {
        Ok(())
    }
}

/// A `LogFileWriter` that shares one underlying writer (and therefore one log
/// file) across multiple `LogTask`s running on different executor threads.
/// The lock is taken per call, so each `store_message` writes a complete
/// entry atomically with respect to the other tasks — entries from different
/// tasks can interleave in the file but never tear. Message serialization
/// happens in the caller's per-channel scratch buffer *before* the sink call,
/// so only the actual write is serialized across tasks.
#[derive(Clone)]
pub struct SharedLogFileWriter {
    inner: Arc<Mutex<Box<dyn LogFileWriter>>>,
}

impl SharedLogFileWriter {
    pub fn new(writer: Box<dyn LogFileWriter>) -> Self {
        SharedLogFileWriter {
            inner: Arc::new(Mutex::new(writer)),
        }
    }
}

impl LogFileWriter for SharedLogFileWriter {
    fn store_message(
        &mut self,
        channel_name: &str,
        header: &MessageHeader,
        body: &[u8],
    ) -> Result<(), BoxedLogError> {
        self.inner
            .lock()
            .expect("shared log writer lock poisoned")
            .store_message(channel_name, header, body)
    }

    fn flush(&mut self) -> Result<(), BoxedLogError> {
        self.inner
            .lock()
            .expect("shared log writer lock poisoned")
            .flush()
    }
}

pub struct LogEntry<'a> {
    pub header: MessageHeader,
    pub channel_name: &'a str,
    pub serialized_body: &'a [u8],
}

pub trait LogFileReader {
    /// Sorts all log entries by time, regardless of channel.
    fn sort_by_time(&mut self);

    /// Number of log entries.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn entry(&self, index: usize) -> Option<LogEntry<'_>>;

    fn iter(&self) -> LogEntryIter<'_>
    where
        Self: Sized,
    {
        LogEntryIter {
            reader: self,
            index: 0,
        }
    }
}

pub struct LogEntryIter<'a> {
    reader: &'a dyn LogFileReader,
    index: usize,
}

impl<'a> Iterator for LogEntryIter<'a> {
    type Item = LogEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.reader.entry(self.index)?;
        self.index += 1;
        Some(entry)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.reader.len().saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

impl<'a> ExactSizeIterator for LogEntryIter<'a> {}

impl fmt::Debug for dyn LogFileWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("dyn LogFileWriter").finish_non_exhaustive()
    }
}
