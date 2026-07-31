use std::collections::HashMap;
use std::io::{BufRead, Write};

use task::message::MessageHeader;

use crate::log_file::{LogEntry, LogFileReader, LogFileWriter};

#[derive(serde::Serialize, serde::Deserialize)]
struct JsonLogEntry {
    header: MessageHeader,
    channel_name: String,
    body: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct JsonLogArtifact {
    artifact: String,
    body: serde_json::Value,
}

/// Discriminated line from the log file: either an artifact or a message.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum JsonLogFileLine {
    Artifact {
        artifact: String,
        body: serde_json::Value,
    },
    Message(JsonLogEntry),
}

pub struct JsonLogFileWriter<W> {
    writer: W,
}

impl<W: Write> JsonLogFileWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: Write + Send> LogFileWriter for JsonLogFileWriter<W> {
    fn store_message(
        &mut self,
        channel_name: &str,
        header: &MessageHeader,
        body: &[u8],
    ) -> Result<(), crate::log_file::BoxedLogError> {
        let entry = JsonLogEntry {
            header: *header,
            channel_name: channel_name.to_owned(),
            body: body.to_vec(),
        };
        serde_json::to_writer(&mut self.writer, &entry)
            .map_err(|e| -> crate::log_file::BoxedLogError { Box::new(e) })?;
        self.writer
            .write_all(b"\n")
            .map_err(|e| -> crate::log_file::BoxedLogError { Box::new(e) })?;
        Ok(())
    }

    fn write_artifact(
        &mut self,
        name: &str,
        body: &[u8],
    ) -> Result<(), crate::log_file::BoxedLogError> {
        let value: serde_json::Value = serde_json::from_slice(body)
            .map_err(|e| -> crate::log_file::BoxedLogError { Box::new(e) })?;
        let artifact = JsonLogArtifact {
            artifact: name.to_owned(),
            body: value,
        };
        serde_json::to_writer(&mut self.writer, &artifact)
            .map_err(|e| -> crate::log_file::BoxedLogError { Box::new(e) })?;
        self.writer
            .write_all(b"\n")
            .map_err(|e| -> crate::log_file::BoxedLogError { Box::new(e) })?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), crate::log_file::BoxedLogError> {
        self.writer
            .flush()
            .map_err(|e| -> crate::log_file::BoxedLogError { Box::new(e) })
    }
}

pub struct JsonLogFileReader {
    entries: Vec<JsonLogEntry>,
    artifacts: HashMap<String, Vec<u8>>,
}

impl JsonLogFileReader {
    pub fn from_reader<R: BufRead>(reader: R) -> Result<Self, serde_json::Error> {
        let mut entries = Vec::new();
        let mut artifacts = HashMap::new();
        for line in reader.lines() {
            let line = line.map_err(serde_json::Error::io)?;
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<JsonLogFileLine>(&line)? {
                JsonLogFileLine::Artifact { artifact, body } => {
                    let bytes = serde_json::to_vec(&body)?;
                    artifacts.insert(artifact, bytes);
                }
                JsonLogFileLine::Message(entry) => {
                    entries.push(entry);
                }
            }
        }
        Ok(Self { entries, artifacts })
    }
}

impl LogFileReader for JsonLogFileReader {
    fn sort_by_time(&mut self) {
        self.entries.sort_by_key(|e| e.header.published_at);
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn entry(&self, index: usize) -> Option<LogEntry<'_>> {
        let e = self.entries.get(index)?;
        Some(LogEntry {
            header: e.header,
            channel_name: &e.channel_name,
            serialized_body: &e.body,
        })
    }

    fn artifact(&self, name: &str) -> Option<&[u8]> {
        self.artifacts.get(name).map(|v| v.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use task::time::FrameworkTime;

    fn make_header(ns: i64) -> MessageHeader {
        MessageHeader::new(FrameworkTime::from_nanoseconds(ns))
    }

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        {
            let mut writer = JsonLogFileWriter::new(&mut buf);
            writer
                .store_message("prices", &make_header(100), b"hello")
                .unwrap();
            writer
                .store_message("orders", &make_header(50), b"world")
                .unwrap();
            writer
                .store_message("prices", &make_header(200), b"!")
                .unwrap();
        }

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        assert_eq!(reader.len(), 3);

        let entries: Vec<_> = reader.iter().collect();
        assert_eq!(entries[0].channel_name, "prices");
        assert_eq!(entries[0].serialized_body, b"hello");
        assert_eq!(
            entries[0].header.published_at,
            FrameworkTime::from_nanoseconds(100)
        );

        assert_eq!(entries[1].channel_name, "orders");
        assert_eq!(entries[1].serialized_body, b"world");

        assert_eq!(entries[2].channel_name, "prices");
        assert_eq!(entries[2].serialized_body, b"!");
    }

    #[test]
    fn sort_by_time() {
        let mut buf = Vec::new();
        {
            let mut writer = JsonLogFileWriter::new(&mut buf);
            writer
                .store_message("b", &make_header(300), b"third")
                .unwrap();
            writer
                .store_message("a", &make_header(100), b"first")
                .unwrap();
            writer
                .store_message("c", &make_header(200), b"second")
                .unwrap();
        }

        let mut reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        reader.sort_by_time();

        let entries: Vec<_> = reader.iter().collect();
        assert_eq!(
            entries[0].header.published_at,
            FrameworkTime::from_nanoseconds(100)
        );
        assert_eq!(
            entries[1].header.published_at,
            FrameworkTime::from_nanoseconds(200)
        );
        assert_eq!(
            entries[2].header.published_at,
            FrameworkTime::from_nanoseconds(300)
        );
    }

    #[test]
    fn empty_log() {
        let reader = JsonLogFileReader::from_reader(std::io::empty()).unwrap();
        assert!(reader.is_empty());
        assert_eq!(reader.iter().count(), 0);
    }
}
