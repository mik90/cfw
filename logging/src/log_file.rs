use std::fmt;

use task::message::MessageHeader;

/// Error type used by all `BoxedLogFileWriter`s. Anything that can fail during
/// logging ultimately surfaces as one of these so the writer can be stored
/// behind a single trait object without exposing a concrete associated type.
pub type BoxedLogError = Box<dyn std::error::Error + Send + Sync>;

pub trait LogFileWriter {
    type Error: std::error::Error + Send + Sync + 'static;
    fn store_message(
        &mut self,
        channel_name: &str,
        header: &MessageHeader,
        body: &[u8],
    ) -> Result<(), Self::Error>;
}

/// Object-safe wrapper around any `LogFileWriter` that boxes its errors so
/// the writer can be stored as `Box<dyn LogFileWriterObj>` on the `LogTask`.
/// `dyn LogFileWriter` itself isn't object-safe because of the associated
/// `Error` type; this trait is the erased equivalent.
pub trait LogFileWriterObj: Send {
    fn store_message(
        &mut self,
        channel_name: &str,
        header: &MessageHeader,
        body: &[u8],
    ) -> Result<(), BoxedLogError>;
}

/// Adapter that wraps a concrete `LogFileWriter` and boxes errors on the way out.
pub struct BoxedLogFileWriter<W: LogFileWriter + Send + 'static>(W);

impl<W: LogFileWriter + Send + 'static> BoxedLogFileWriter<W> {
    pub fn new(inner: W) -> Self {
        BoxedLogFileWriter(inner)
    }

    pub fn into_boxed(self) -> Box<dyn LogFileWriterObj> {
        Box::new(self)
    }
}

impl<W: LogFileWriter + Send + 'static> LogFileWriterObj for BoxedLogFileWriter<W> {
    fn store_message(
        &mut self,
        channel_name: &str,
        header: &MessageHeader,
        body: &[u8],
    ) -> Result<(), BoxedLogError> {
        self.0
            .store_message(channel_name, header, body)
            .map_err(|e| -> BoxedLogError { Box::new(e) })
    }
}

/// fmt::Debug impl is required so `Box<dyn LogFileWriterObj>` can be embedded
/// in structs that derive Debug without forcing the inner writer to be Debug.
impl<W: LogFileWriter + Send + 'static> fmt::Debug for BoxedLogFileWriter<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxedLogFileWriter").finish_non_exhaustive()
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

    fn get_entry(&self, index: usize) -> Option<LogEntry<'_>>;

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
        let entry = self.reader.get_entry(self.index)?;
        self.index += 1;
        Some(entry)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.reader.len().saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

impl<'a> ExactSizeIterator for LogEntryIter<'a> {}
