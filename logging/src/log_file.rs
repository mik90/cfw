use task::message::MessageHeader;

pub trait LogFileWriter {
    type Error;
    fn store_message(
        &mut self,
        channel_name: &str,
        header: &MessageHeader,
        body: &[u8],
    ) -> Result<(), Self::Error>;
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
