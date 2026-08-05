use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use task::callback::PortMut;
use task::channel_registry::{ChannelPublisherWriter, ChannelRegistry, DeserializerFn};
use task::execution_log::EXECUTION_LOG_CHANNEL;
use task::generic_publisher::GenericPublisher;
use task::message::MessageHeader;
use task::pub_sub::ChannelName;
use task::task_graph_builder::TaskGraphBuildStepError;
use task::time::FrameworkTime;

static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

fn temp_path(kind: &str) -> PathBuf {
    let pid = std::process::id();
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("cfw_replay_{kind}_{pid}_{id}"))
}

/// An owned log entry suitable for replay. Carries the header, channel name,
/// and serialized message body.
#[derive(Clone, Debug)]
pub struct OwnedLogEntry {
    pub header: MessageHeader,
    pub channel_name: ChannelName,
    pub serialized_body: Vec<u8>,
}

// Discriminated line parser for the input log file.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum RawLine {
    Artifact {
        artifact: String,
    },
    Message {
        header: MessageHeader,
        channel_name: String,
        body: Vec<u8>,
    },
}

// Helper for serializing entries to temp run files.
#[derive(serde::Serialize)]
struct WriteEntry<'a> {
    header: MessageHeader,
    channel_name: &'a str,
    body: &'a [u8],
}

// Helper for deserializing entries from temp run files.
#[derive(serde::Deserialize)]
struct ReadEntry {
    header: MessageHeader,
    channel_name: String,
    body: Vec<u8>,
}

fn parse_entry_line(line: &str) -> Option<OwnedLogEntry> {
    if line.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<RawLine>(line) {
        Ok(RawLine::Artifact { .. }) => None,
        Ok(RawLine::Message {
            header,
            channel_name,
            body,
        }) => Some(OwnedLogEntry {
            header,
            channel_name,
            serialized_body: body,
        }),
        Err(_) => None,
    }
}

fn write_entry<W: Write>(w: &mut W, entry: &OwnedLogEntry) -> Result<(), serde_json::Error> {
    let line = serde_json::to_string(&WriteEntry {
        header: entry.header,
        channel_name: &entry.channel_name,
        body: &entry.serialized_body,
    })?;
    w.write_all(line.as_bytes())
        .map_err(serde_json::Error::io)?;
    w.write_all(b"\n").map_err(serde_json::Error::io)
}

fn write_entries(path: &Path, entries: &[OwnedLogEntry]) -> Result<(), serde_json::Error> {
    let mut file = File::create(path).map_err(serde_json::Error::io)?;
    for entry in entries {
        write_entry(&mut file, entry)?;
    }
    Ok(())
}

fn read_run_entry(reader: &mut BufReader<File>) -> Option<OwnedLogEntry> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                None
            } else {
                serde_json::from_str::<ReadEntry>(trimmed)
                    .ok()
                    .map(|r| OwnedLogEntry {
                        header: r.header,
                        channel_name: r.channel_name,
                        serialized_body: r.body,
                    })
            }
        }
    }
}

/// Merge entry used in the binary heap during external merge.
struct MergeEntry {
    time: FrameworkTime,
    run_index: usize,
    entry: OwnedLogEntry,
}

impl PartialEq for MergeEntry {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time
    }
}

impl Eq for MergeEntry {}

impl PartialOrd for MergeEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MergeEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.time.cmp(&other.time)
    }
}

/// Where the reader currently streams entries from.
enum SortedSource {
    /// Log was empty.
    Empty,
    /// Entries streamed directly from an already-sorted file.
    FromFile(BufReader<File>),
}

/// A bounded-memory, time-sorted streaming reader for JSON log files.
///
/// The log is read in a streaming fashion; memory is proportional to the
/// number of distinct channel names plus `sort_batch_size` entries, never the
/// whole log. If the input is already time-sorted the entries are streamed
/// directly; otherwise an external merge sort spills sorted chunks to
/// temporary files and merges them.
///
/// Callers advance through the log with [`read_until`](Self::read_until), which
/// returns all entries with `published_at <= time` and the timestamp of the
/// next entry (the "T+1" time).
pub struct SortedLogStreamReader {
    source: SortedSource,
    channels: HashSet<ChannelName>,
    entry_count: usize,
    first_time: Option<FrameworkTime>,
    peeked: Option<OwnedLogEntry>,
    temp_files: Vec<PathBuf>,
}

impl SortedLogStreamReader {
    /// Build a streaming reader from a log file path.
    ///
    /// `sort_batch_size` controls the maximum number of entries held in memory
    /// at once during the external sort path. Larger values improve I/O
    /// performance at the cost of memory.
    pub fn from_path(path: &Path, sort_batch_size: usize) -> Result<Self, serde_json::Error> {
        let mut channels: HashSet<ChannelName> = HashSet::new();
        let mut entry_count = 0usize;
        let mut first_time: Option<FrameworkTime> = None;
        let mut already_sorted = true;
        let mut prev_time: Option<FrameworkTime> = None;

        {
            let file = File::open(path).map_err(serde_json::Error::io)?;
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let line = line.map_err(serde_json::Error::io)?;
                let Some(entry) = parse_entry_line(&line) else {
                    continue;
                };
                let t = entry.header.published_at;
                channels.insert(entry.channel_name);
                if first_time.is_none() {
                    first_time = Some(t);
                }
                entry_count += 1;
                if already_sorted {
                    if let Some(prev) = prev_time
                        && t < prev
                    {
                        already_sorted = false;
                    }
                    prev_time = Some(t);
                }
            }
        }

        if entry_count == 0 {
            return Ok(SortedLogStreamReader {
                source: SortedSource::Empty,
                channels,
                entry_count,
                first_time,
                peeked: None,
                temp_files: vec![],
            });
        }

        if already_sorted {
            let file = File::open(path).map_err(serde_json::Error::io)?;
            let mut reader = SortedLogStreamReader {
                source: SortedSource::FromFile(BufReader::new(file)),
                channels,
                entry_count,
                first_time,
                peeked: None,
                temp_files: vec![],
            };
            reader.advance_peek();
            return Ok(reader);
        }

        let sorted_path = external_sort(path, sort_batch_size)?;
        let file = File::open(&sorted_path).map_err(serde_json::Error::io)?;
        let mut reader = SortedLogStreamReader {
            source: SortedSource::FromFile(BufReader::new(file)),
            channels,
            entry_count,
            first_time,
            peeked: None,
            temp_files: vec![sorted_path],
        };
        reader.advance_peek();
        Ok(reader)
    }

    /// Build a streaming reader from any `BufRead` source.
    ///
    /// The source is copied to a temporary file so it can be streamed
    /// multiple times; prefer [`from_path`](Self::from_path) for real log
    /// files to avoid the copy.
    pub fn from_reader<R: BufRead>(
        reader: R,
        sort_batch_size: usize,
    ) -> Result<Self, serde_json::Error> {
        let path = temp_path("input");
        {
            let mut file = File::create(&path).map_err(serde_json::Error::io)?;
            for line in reader.lines() {
                let line = line.map_err(serde_json::Error::io)?;
                writeln!(file, "{line}").map_err(serde_json::Error::io)?;
            }
        }
        let mut result = Self::from_path(&path, sort_batch_size)?;
        result.temp_files.push(path);
        Ok(result)
    }

    fn advance_peek(&mut self) {
        self.peeked = match &mut self.source {
            SortedSource::Empty => None,
            SortedSource::FromFile(reader) => read_run_entry(reader),
        };
    }

    /// Distinct channel names discovered in the log.
    pub fn channel_names(&self) -> &HashSet<ChannelName> {
        &self.channels
    }

    /// Whether the log contains zero entries.
    pub fn is_empty(&self) -> bool {
        self.entry_count == 0
    }

    /// Timestamp of the earliest entry, or `None` if empty.
    pub fn first_log_time(&self) -> Option<FrameworkTime> {
        self.first_time
    }

    /// Timestamp of the next un-yielded entry, or `None` if exhausted.
    pub fn peek_time(&mut self) -> Option<FrameworkTime> {
        self.peeked.as_ref().map(|e| e.header.published_at)
    }

    /// Consume and return all not-yet-yielded entries with `published_at <= time`.
    ///
    /// Returns `(batch, next_time)` where `batch` is the entries for the
    /// current time step and `next_time` is the timestamp of the first entry
    /// with `published_at > time` (the "T+1" step), or `None` at EOF.
    pub fn read_until(
        &mut self,
        time: FrameworkTime,
    ) -> (Vec<OwnedLogEntry>, Option<FrameworkTime>) {
        let mut batch = Vec::new();
        loop {
            match self.peeked.take() {
                Some(entry) if entry.header.published_at <= time => {
                    batch.push(entry);
                    self.advance_peek();
                }
                Some(entry) => {
                    self.peeked = Some(entry);
                    let next = self.peeked.as_ref().map(|e| e.header.published_at);
                    return (batch, next);
                }
                None => {
                    return (batch, None);
                }
            }
        }
    }
}

impl Drop for SortedLogStreamReader {
    fn drop(&mut self) {
        for path in self.temp_files.drain(..) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Sort an unsorted log file into a single time-sorted temp file.
///
/// Memory is bounded by `sort_batch_size` entries during the chunking pass
/// and one entry per run during the merge.
fn external_sort(path: &Path, sort_batch_size: usize) -> Result<PathBuf, serde_json::Error> {
    let batch = sort_batch_size.max(1);
    let mut run_paths: Vec<PathBuf> = Vec::new();

    // Phase 1: read sorted chunks and spill each to a temp run file.
    let file = File::open(path).map_err(serde_json::Error::io)?;
    let mut reader = BufReader::new(file);
    let mut chunk: Vec<OwnedLogEntry> = Vec::with_capacity(batch);
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(serde_json::Error::io)?;
        if n == 0 {
            break;
        }
        if let Some(entry) = parse_entry_line(&line) {
            chunk.push(entry);
            if chunk.len() >= batch {
                chunk.sort_by_key(|e| e.header.published_at);
                let run_path = temp_path("run");
                write_entries(&run_path, &chunk)?;
                run_paths.push(run_path);
                chunk.clear();
            }
        }
    }
    if !chunk.is_empty() {
        chunk.sort_by_key(|e| e.header.published_at);
        let run_path = temp_path("run");
        write_entries(&run_path, &chunk)?;
        run_paths.push(run_path);
    }

    // Phase 2: k-way merge the runs into one sorted file.
    let sorted_path = temp_path("sorted");
    let mut out = File::create(&sorted_path).map_err(serde_json::Error::io)?;
    let mut readers: Vec<BufReader<File>> = Vec::with_capacity(run_paths.len());
    for p in &run_paths {
        readers.push(BufReader::new(
            File::open(p).map_err(serde_json::Error::io)?,
        ));
    }

    let mut heap: BinaryHeap<Reverse<MergeEntry>> = BinaryHeap::new();
    for (idx, r) in readers.iter_mut().enumerate() {
        if let Some(entry) = read_run_entry(r) {
            heap.push(Reverse(MergeEntry {
                time: entry.header.published_at,
                run_index: idx,
                entry,
            }));
        }
    }
    while let Some(Reverse(item)) = heap.pop() {
        write_entry(&mut out, &item.entry)?;
        if let Some(entry) = read_run_entry(&mut readers[item.run_index]) {
            heap.push(Reverse(MergeEntry {
                time: entry.header.published_at,
                run_index: item.run_index,
                entry,
            }));
        }
    }

    for p in &run_paths {
        let _ = std::fs::remove_file(p);
    }
    Ok(sorted_path)
}

/// A sink for replaying one channel's messages: deserializer + publisher pair.
pub struct ReplaySink {
    pub channel_name: ChannelName,
    pub deserializer: DeserializerFn,
    pub publisher: Box<dyn GenericPublisher>,
    pub writer: ChannelPublisherWriter,
}

/// Map from channel name to its [`ReplaySink`].
pub struct ReplaySinkMap {
    sinks: HashMap<ChannelName, ReplaySink>,
}

impl ReplaySinkMap {
    /// Create an empty sink map.
    pub fn new() -> Self {
        ReplaySinkMap {
            sinks: HashMap::new(),
        }
    }

    fn get_mut(&mut self, channel: &str) -> Option<&mut ReplaySink> {
        self.sinks.get_mut(channel)
    }

    fn insert(&mut self, key: ChannelName, value: ReplaySink) -> Option<ReplaySink> {
        self.sinks.insert(key, value)
    }

    /// Check if a channel is present in the map.
    pub fn contains_key(&self, key: &str) -> bool {
        self.sinks.contains_key(key)
    }

    /// Returns `true` if the map contains no sinks.
    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }

    /// Returns the number of sinks.
    pub fn len(&self) -> usize {
        self.sinks.len()
    }

    /// Publish a log entry through the matching sink.
    pub fn publish(&mut self, entry: &OwnedLogEntry) {
        if let Some(sink) = self.get_mut(&entry.channel_name)
            && let Ok(value) = (sink.deserializer)(&entry.serialized_body)
        {
            (sink.writer)(&mut *sink.publisher, value);
        }
    }

    /// Invoke `f` for every publisher.
    pub fn for_each_publisher<'a>(&'a self, f: &mut dyn FnMut(&'a dyn GenericPublisher)) {
        for sink in self.sinks.values() {
            f(sink.publisher.as_ref());
        }
    }

    /// Invoke `f` for every mutable publisher.
    pub fn for_each_publisher_mut<'a>(
        &'a mut self,
        f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
    ) {
        for sink in self.sinks.values_mut() {
            f(sink.publisher.as_mut());
        }
    }

    /// Invoke `f` for every port (publisher only).
    pub fn for_each_port_mut<'a>(&'a mut self, f: &mut dyn FnMut(PortMut<'a>)) {
        for sink in self.sinks.values_mut() {
            f(PortMut::Publisher(sink.publisher.as_mut()));
        }
    }
}

impl Default for ReplaySinkMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a [`ReplaySinkMap`] from a [`SortedLogStreamReader`]'s channel names.
///
/// Skips `EXECUTION_LOG_CHANNEL` and any channels in `denylist`. Returns an
/// error if a log channel appears that is not registered in the registry.
pub fn build_replay_sinks(
    reader: &SortedLogStreamReader,
    registry: &ChannelRegistry,
    denylist: &HashSet<ChannelName>,
) -> Result<ReplaySinkMap, TaskGraphBuildStepError> {
    let mut map = ReplaySinkMap::new();
    for channel in reader.channel_names() {
        if channel.as_str() == EXECUTION_LOG_CHANNEL {
            continue;
        }
        if denylist.contains(channel) {
            continue;
        }
        let Some(type_id) = registry.channel_type(channel) else {
            return Err(format!(
                "replay: channel '{channel}' appears in log but was not registered via ChannelRegistry::register_channel"
            )
            .into());
        };
        let Some(factory) = registry.channel_publisher_factory(type_id) else {
            return Err(format!(
                "replay: no publisher factory for channel '{channel}' type {type_id:?}"
            )
            .into());
        };
        let (publisher, writer) = factory(channel.clone());
        let Some(deserializer) = registry.deserializer_for(type_id) else {
            continue;
        };
        map.insert(
            channel.clone(),
            ReplaySink {
                channel_name: channel.clone(),
                deserializer,
                publisher,
                writer,
            },
        );
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_file::LogFileWriter;
    use crate::log_file_json::JsonLogFileWriter;
    use task::time::FrameworkTime;

    fn make_header(ns: i64) -> MessageHeader {
        MessageHeader::new(FrameworkTime::from_nanoseconds(ns))
    }

    fn write_log(entries: &[(i64, &str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = JsonLogFileWriter::new(&mut buf);
            for (ns, channel, body) in entries {
                writer
                    .store_message(channel, &make_header(*ns), body)
                    .unwrap();
            }
        }
        buf
    }

    #[test]
    fn test_sorted_input_yields_in_order() {
        let data = write_log(&[(100, "a", b"x"), (200, "b", b"y"), (300, "c", b"z")]);
        let mut reader = SortedLogStreamReader::from_reader(data.as_slice(), 64).unwrap();
        assert!(!reader.is_empty());
        assert_eq!(
            reader.first_log_time(),
            Some(FrameworkTime::from_nanoseconds(100))
        );

        let (batch, next) = reader.read_until(FrameworkTime::from_nanoseconds(200));
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].channel_name, "a");
        assert_eq!(batch[1].channel_name, "b");
        assert_eq!(next, Some(FrameworkTime::from_nanoseconds(300)));

        let (batch, next) = reader.read_until(FrameworkTime::from_nanoseconds(300));
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].channel_name, "c");
        assert_eq!(next, None);
    }

    #[test]
    fn test_unsorted_input_yields_sorted() {
        let data = write_log(&[(300, "c", b"z"), (100, "a", b"x"), (200, "b", b"y")]);
        let mut reader = SortedLogStreamReader::from_reader(data.as_slice(), 2).unwrap();

        let (batch, next) = reader.read_until(FrameworkTime::from_nanoseconds(100));
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].channel_name, "a");
        assert_eq!(next, Some(FrameworkTime::from_nanoseconds(200)));

        let (batch, next) = reader.read_until(FrameworkTime::from_nanoseconds(200));
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].channel_name, "b");
        assert_eq!(next, Some(FrameworkTime::from_nanoseconds(300)));

        let (batch, next) = reader.read_until(FrameworkTime::from_nanoseconds(300));
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].channel_name, "c");
        assert_eq!(next, None);
    }

    #[test]
    fn test_channel_names() {
        let data = write_log(&[(100, "a", b"x"), (200, "b", b"y"), (300, "a", b"z")]);
        let reader = SortedLogStreamReader::from_reader(data.as_slice(), 64).unwrap();
        let mut names: Vec<&String> = reader.channel_names().iter().collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn test_read_until_semantics() {
        let data = write_log(&[(1000, "x", b"1"), (3000, "x", b"2"), (5000, "x", b"3")]);
        let mut reader = SortedLogStreamReader::from_reader(data.as_slice(), 64).unwrap();

        assert_eq!(
            reader.peek_time(),
            Some(FrameworkTime::from_nanoseconds(1000))
        );

        let (batch, next) = reader.read_until(FrameworkTime::from_nanoseconds(1000));
        assert_eq!(batch.len(), 1);
        assert_eq!(next, Some(FrameworkTime::from_nanoseconds(3000)));

        assert_eq!(
            reader.peek_time(),
            Some(FrameworkTime::from_nanoseconds(3000))
        );

        let (batch, next) = reader.read_until(FrameworkTime::from_nanoseconds(2000));
        assert_eq!(batch.len(), 0);
        assert_eq!(next, Some(FrameworkTime::from_nanoseconds(3000)));

        let (batch, next) = reader.read_until(FrameworkTime::from_nanoseconds(3000));
        assert_eq!(batch.len(), 1);
        assert_eq!(next, Some(FrameworkTime::from_nanoseconds(5000)));

        let (batch, next) = reader.read_until(FrameworkTime::from_nanoseconds(9999));
        assert_eq!(batch.len(), 1);
        assert_eq!(next, None);

        assert!(reader.peek_time().is_none());
    }

    #[test]
    fn test_empty_log() {
        let data = Vec::new();
        let mut reader = SortedLogStreamReader::from_reader(data.as_slice(), 64).unwrap();
        assert!(reader.is_empty());
        assert_eq!(reader.first_log_time(), None);
        assert!(reader.peek_time().is_none());
        let (batch, next) = reader.read_until(FrameworkTime::from_nanoseconds(0));
        assert!(batch.is_empty());
        assert!(next.is_none());
    }

    #[test]
    fn test_peek_time_consistency() {
        let data = write_log(&[(10, "a", b"v1"), (20, "a", b"v2")]);
        let mut reader = SortedLogStreamReader::from_reader(data.as_slice(), 64).unwrap();
        assert_eq!(
            reader.peek_time(),
            Some(FrameworkTime::from_nanoseconds(10))
        );
        let (batch, _) = reader.read_until(FrameworkTime::from_nanoseconds(10));
        assert_eq!(batch.len(), 1);
        assert_eq!(
            reader.peek_time(),
            Some(FrameworkTime::from_nanoseconds(20))
        );
        let (batch, _) = reader.read_until(FrameworkTime::from_nanoseconds(20));
        assert_eq!(batch.len(), 1);
        assert!(reader.peek_time().is_none());
    }

    #[test]
    fn test_build_replay_sinks_basic() {
        let data = write_log(&[(100, "integer", &42u64.to_le_bytes())]);
        let reader = SortedLogStreamReader::from_reader(data.as_slice(), 64).unwrap();

        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("integer".into());

        let sinks = build_replay_sinks(&reader, &registry, &HashSet::new()).unwrap();
        assert_eq!(sinks.len(), 1);
        assert!(sinks.contains_key("integer"));
    }

    #[test]
    fn test_build_replay_sinks_denylist() {
        let data = write_log(&[(100, "integer", &42u64.to_le_bytes())]);
        let reader = SortedLogStreamReader::from_reader(data.as_slice(), 64).unwrap();

        let mut registry = ChannelRegistry::new();
        registry.register_channel::<u64>("integer".into());

        let mut deny = HashSet::new();
        deny.insert("integer".to_string());
        let sinks = build_replay_sinks(&reader, &registry, &deny).unwrap();
        assert!(sinks.is_empty());
    }
}
