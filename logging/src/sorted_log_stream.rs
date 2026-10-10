//! Strict, stable external sorting of the current JSON-lines log format.
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use task::{message::MessageHeader, time::FrameworkTime};

#[derive(Debug)]
pub enum LogReadError {
    Io(std::io::Error),
    Json {
        line: usize,
        source: serde_json::Error,
    },
    CorruptRun(serde_json::Error),
    Invalid(String),
    Poisoned,
}
impl std::fmt::Display for LogReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "log IO: {e}"),
            Self::Json { line, source } => write!(f, "invalid log line {line}: {source}"),
            Self::CorruptRun(e) => write!(f, "invalid sorted run: {e}"),
            Self::Invalid(reason) => f.write_str(reason),
            Self::Poisoned => f.write_str("log reader failed previously"),
        }
    }
}
impl std::error::Error for LogReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json { source, .. } | Self::CorruptRun(source) => Some(source),
            _ => None,
        }
    }
}
impl From<std::io::Error> for LogReadError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedLogEntry {
    pub header: MessageHeader,
    pub channel_name: String,
    #[serde(rename = "body")]
    pub serialized_body: Vec<u8>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    artifact: String,
    body: serde_json::Value,
}
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Row {
    Artifact(Artifact),
    Message(OwnedLogEntry),
}
#[derive(serde::Serialize, serde::Deserialize)]
struct RunEntry {
    sequence: usize,
    entry: OwnedLogEntry,
}
impl RunEntry {
    fn key(&self) -> (FrameworkTime, usize) {
        (self.entry.header.published_at, self.sequence)
    }
}

struct TempRun {
    path: PathBuf,
}
impl Drop for TempRun {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
fn temporary() -> Result<(TempRun, File), LogReadError> {
    loop {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("cfw_sorted_{}_{}", std::process::id(), id));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((TempRun { path }, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
}
fn write_entry(writer: &mut impl Write, entry: &RunEntry) -> Result<(), LogReadError> {
    serde_json::to_writer(&mut *writer, entry).map_err(LogReadError::CorruptRun)?;
    writer.write_all(b"\n")?;
    Ok(())
}
fn read_entry(reader: &mut impl BufRead) -> Result<Option<RunEntry>, LogReadError> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line)
        .map(Some)
        .map_err(LogReadError::CorruptRun)
}
fn spill(chunk: &mut Vec<RunEntry>) -> Result<TempRun, LogReadError> {
    chunk.sort_by_key(RunEntry::key);
    let (run, file) = temporary()?;
    let mut writer = BufWriter::new(file);
    for entry in chunk.drain(..) {
        write_entry(&mut writer, &entry)?;
    }
    writer.flush()?;
    Ok(run)
}
fn merge(left: TempRun, right: TempRun) -> Result<TempRun, LogReadError> {
    let mut a = BufReader::new(File::open(&left.path)?);
    let mut b = BufReader::new(File::open(&right.path)?);
    let (run, file) = temporary()?;
    let mut writer = BufWriter::new(file);
    let mut first = read_entry(&mut a)?;
    let mut second = read_entry(&mut b)?;
    while first.is_some() || second.is_some() {
        if second.is_none()
            || first
                .as_ref()
                .is_some_and(|x| x.key() <= second.as_ref().unwrap().key())
        {
            write_entry(&mut writer, &first.take().unwrap())?;
            first = read_entry(&mut a)?;
        } else {
            write_entry(&mut writer, &second.take().unwrap())?;
            second = read_entry(&mut b)?;
        }
    }
    writer.flush()?;
    Ok(run)
}
fn add_run(levels: &mut Vec<Option<TempRun>>, mut run: TempRun) -> Result<(), LogReadError> {
    for level in levels.iter_mut() {
        match level.take() {
            Some(previous) => run = merge(previous, run)?,
            None => {
                *level = Some(run);
                return Ok(());
            }
        }
    }
    levels.push(Some(run));
    Ok(())
}
fn sort_run(input: TempRun, batch_size: usize) -> Result<TempRun, LogReadError> {
    let mut reader = BufReader::new(File::open(&input.path)?);
    let mut levels = Vec::new();
    let mut chunk = Vec::new();
    while let Some(entry) = read_entry(&mut reader)? {
        chunk.push(entry);
        if chunk.len() == batch_size {
            add_run(&mut levels, spill(&mut chunk)?)?;
        }
    }
    if !chunk.is_empty() {
        add_run(&mut levels, spill(&mut chunk)?)?;
    }
    let mut result = None;
    for run in levels.into_iter().flatten() {
        result = Some(match result {
            Some(previous) => merge(previous, run)?,
            None => run,
        });
    }
    result.ok_or_else(|| LogReadError::Invalid("empty external sort input".into()))
}
enum Source {
    Empty,
    Memory(std::vec::IntoIter<OwnedLogEntry>),
    // Close the file before releasing its path, including on early reader drop.
    File {
        reader: BufReader<File>,
        _run: TempRun,
    },
}

/// Bounded-memory snapshot of a JSON-lines log. Sorting holds at most one chunk
/// and two merge heads; a binary merge hierarchy bounds open file descriptors.
/// Already sorted inputs use a single snapshot pass without merge sorting.
/// Metadata/channel dictionaries are retained. Equal times preserve file order.
pub struct SortedLogStreamReader {
    source: Source,
    channels: HashSet<String>,
    artifacts: HashMap<String, Vec<u8>>,
    entry_count: usize,
    first_time: Option<FrameworkTime>,
    peeked: Option<OwnedLogEntry>,
    failed: bool,
}
impl SortedLogStreamReader {
    pub fn from_path(path: &Path, sort_batch_size: usize) -> Result<Self, LogReadError> {
        Self::from_reader(BufReader::new(File::open(path)?), sort_batch_size)
    }
    pub fn from_reader(
        mut input: impl BufRead,
        sort_batch_size: usize,
    ) -> Result<Self, LogReadError> {
        if sort_batch_size == 0 {
            return Err(LogReadError::Invalid(
                "sort batch size must be positive".into(),
            ));
        }
        let mut channels = HashSet::new();
        let mut artifacts = HashMap::new();
        let mut count = 0_usize;
        let mut line_number = 0;
        let mut line = String::new();
        let (snapshot, file) = temporary()?;
        let mut writer = BufWriter::new(file);
        let mut sorted = true;
        let mut previous = None;
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                break;
            }
            line_number += 1;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Row>(&line).map_err(|source| LogReadError::Json {
                line: line_number,
                source,
            })? {
                Row::Artifact(artifact) => {
                    if artifacts.contains_key(&artifact.artifact)
                        && artifact.artifact
                            != crate::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT
                    {
                        return Err(LogReadError::Invalid(format!(
                            "duplicate artifact '{}' at line {line_number}",
                            artifact.artifact
                        )));
                    }
                    artifacts.insert(
                        artifact.artifact,
                        serde_json::to_vec(&artifact.body).map_err(|source| {
                            LogReadError::Json {
                                line: line_number,
                                source,
                            }
                        })?,
                    );
                }
                Row::Message(entry) => {
                    validate(&entry)?;
                    channels.insert(entry.channel_name.clone());
                    let time = entry.header.published_at;
                    sorted &= previous.is_none_or(|last| last <= time);
                    previous = Some(time);
                    write_entry(
                        &mut writer,
                        &RunEntry {
                            sequence: count,
                            entry,
                        },
                    )?;
                    count = count
                        .checked_add(1)
                        .ok_or_else(|| LogReadError::Invalid("too many log entries".into()))?;
                }
            }
        }
        writer.flush()?;
        drop(writer);
        let source = if count == 0 {
            Source::Empty
        } else {
            let run = if sorted {
                snapshot
            } else {
                sort_run(snapshot, sort_batch_size)?
            };
            Source::File {
                reader: BufReader::new(File::open(&run.path)?),
                _run: run,
            }
        };
        Self::initialize(source, channels, artifacts, count)
    }
    /// In-memory fixture alternative with the same stable ordering and validation.
    pub fn from_entries(
        mut entries: Vec<OwnedLogEntry>,
        artifacts: HashMap<String, Vec<u8>>,
    ) -> Result<Self, LogReadError> {
        let mut channels = HashSet::new();
        for entry in &entries {
            validate(entry)?;
            channels.insert(entry.channel_name.clone());
        }
        for (name, body) in &artifacts {
            serde_json::from_slice::<serde_json::Value>(body)
                .map_err(|e| LogReadError::Invalid(format!("artifact '{name}': {e}")))?;
        }
        entries.sort_by_key(|entry| entry.header.published_at);
        let count = entries.len();
        Self::initialize(
            Source::Memory(entries.into_iter()),
            channels,
            artifacts,
            count,
        )
    }
    fn initialize(
        source: Source,
        channels: HashSet<String>,
        artifacts: HashMap<String, Vec<u8>>,
        entry_count: usize,
    ) -> Result<Self, LogReadError> {
        let mut reader = Self {
            source,
            channels,
            artifacts,
            entry_count,
            first_time: None,
            peeked: None,
            failed: false,
        };
        reader.advance()?;
        reader.first_time = reader.peek_time();
        Ok(reader)
    }
    fn advance(&mut self) -> Result<(), LogReadError> {
        let next = match &mut self.source {
            Source::Empty => Ok(None),
            Source::Memory(entries) => Ok(entries.next()),
            Source::File { reader, .. } => {
                read_entry(reader).map(|entry| entry.map(|row| row.entry))
            }
        };
        match next {
            Ok(entry) => {
                self.peeked = entry;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
    pub fn channel_names(&self) -> &HashSet<String> {
        &self.channels
    }
    pub fn artifact(&self, name: &str) -> Option<&[u8]> {
        self.artifacts.get(name).map(Vec::as_slice)
    }
    pub fn len(&self) -> usize {
        self.entry_count
    }
    pub fn is_empty(&self) -> bool {
        self.entry_count == 0
    }
    pub fn first_log_time(&self) -> Option<FrameworkTime> {
        self.first_time
    }
    pub fn peek_time(&self) -> Option<FrameworkTime> {
        self.peeked.as_ref().map(|e| e.header.published_at)
    }
    pub fn next_entry(&mut self) -> Result<Option<OwnedLogEntry>, LogReadError> {
        if self.failed {
            return Err(LogReadError::Poisoned);
        }
        let current = self.peeked.take();
        self.advance()?;
        Ok(current)
    }
    pub fn read_until(
        &mut self,
        time: FrameworkTime,
    ) -> Result<(Vec<OwnedLogEntry>, Option<FrameworkTime>), LogReadError> {
        if self.failed {
            return Err(LogReadError::Poisoned);
        }
        let mut batch = Vec::new();
        while self.peek_time().is_some_and(|next| next <= time) {
            batch.push(self.next_entry()?.unwrap());
        }
        Ok((batch, self.peek_time()))
    }
}
fn validate(entry: &OwnedLogEntry) -> Result<(), LogReadError> {
    if entry.header.published_at == FrameworkTime::INVALID {
        return Err(LogReadError::Invalid(format!(
            "invalid timestamp on '{}'",
            entry.channel_name
        )));
    }
    if entry.channel_name.is_empty() {
        return Err(LogReadError::Invalid("empty log channel name".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg_attr(miri, ignore = "requires filesystem")]
    fn temporary_run_cleanup_is_scoped() {
        let path = {
            let (run, _file) = temporary().unwrap();
            assert!(run.path.exists());
            run.path.clone()
        };
        assert!(!path.exists());
    }
}
