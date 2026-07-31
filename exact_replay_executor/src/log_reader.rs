//! Parses a log file and extracts the execution log descriptor + ordinary log
//! entries needed for exact replay.
//!
//! # Determinism guarantees
//!
//! 1. Split execution entries (more than [`MESSAGES_PER_ENTRY`] messages) are
//!    grouped by `(callback_node_index, execution_time)` **in the order they
//!    appear in the log** (insertion order via `Vec` + index map), then sorted
//!    by `execution_time` with stable ordering for equal times.
//! 2. Ordinary-log payloads are matched to execution-log headers only after
//!    groups are in replay order, so same-channel/same-timestamp payloads are
//!    consumed deterministically (FIFO per channel).

use std::collections::HashMap;

use logging::log_file::LogFileReader;
use task::execution_log::{
    EXECUTION_LOG_CHANNEL, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, ExecutionLogDescriptor,
    ExecutionLogEntry, ExecutionLogMessage,
};
use task::loggable::Loggable;
use task::message::MessageHeader;
use task::pub_sub::ChannelName;
use task::time::FrameworkTime;

use crate::error::ReplayError;

/// A single replayed execution: the callback node index, the time it executed,
/// the received messages (by subscriber ordinal), and the published messages
/// (by publisher ordinal with their serialized bytes from the ordinary log).
#[derive(Debug, Clone)]
pub(crate) struct ReplayExecution {
    pub callback_node_index: usize,
    pub execution_time: FrameworkTime,
    pub execution_duration_ns: u64,
    /// Messages received by each subscriber ordinal during this execution.
    /// Key: subscriber ordinal, Value: (header, serialized_body)
    pub received: HashMap<u16, Vec<(MessageHeader, Vec<u8>)>>,
    /// Messages published by each publisher ordinal during this execution.
    /// Key: publisher ordinal, Value: (header, serialized_body)
    pub published: HashMap<u16, Vec<(MessageHeader, Vec<u8>)>>,
}

/// Parsed log data: the execution log descriptor, a time-ordered list of
/// replay executions, and any executions that were skipped because they
/// reference a node with no descriptor entry (expected for infrastructure
/// nodes added after the descriptor was generated).
#[derive(Debug)]
pub(crate) struct ReplayLog {
    pub descriptor: ExecutionLogDescriptor,
    pub executions: Vec<ReplayExecution>,
    /// `(node_index, execution_time)` pairs for executions whose node has no
    /// descriptor entry. The executor filters these against known
    /// infrastructure nodes; any that are not infrastructure are an error.
    pub descriptor_less_executions: Vec<(usize, FrameworkTime)>,
}

/// A group of split entries for one execution, tracking insertion order so
/// that equal-timestamp groups remain in log order.
struct ExecutionGroup {
    entries: Vec<ExecutionLogEntry>,
    /// The `(callback_node_index, execution_time_ns)` key.
    key: (usize, i64),
}

/// Parse a log file reader and extract the execution log descriptor and
/// replay executions.
pub(crate) fn parse_replay_log(reader: &dyn LogFileReader) -> Result<ReplayLog, ReplayError> {
    // ── Phase 1: collect raw data ──────────────────────────────────────
    let descriptor: ExecutionLogDescriptor = reader
        .artifact(EXECUTION_LOG_DESCRIPTOR_ARTIFACT)
        .ok_or_else(|| {
            ReplayError::MissingOrInvalidDescriptor(
                "no execution log descriptor artifact found in log file".to_owned(),
            )
        })
        .and_then(|bytes| {
            serde_json::from_slice(bytes).map_err(|e| {
                ReplayError::MissingOrInvalidDescriptor(format!(
                    "failed to parse execution log descriptor: {e}"
                ))
            })
        })?;

    let mut execution_log_entries: Vec<ExecutionLogMessage> = Vec::new();
    // Ordinary log entries, indexed by `(channel, published_at_ns)` with the
    // ordered bodies of every message that shares that key. A message is
    // recorded once here but referenced twice by the execution log — once by
    // the producer (Published) and once by the consumer (Received) — so the
    // payload is resolved by indexed lookup rather than consumed.
    let mut ordinary_log: HashMap<(ChannelName, i64), Vec<Vec<u8>>> = HashMap::new();

    let len = reader.len();
    for i in 0..len {
        let Some(entry) = reader.entry(i) else {
            continue;
        };
        if entry.channel_name == EXECUTION_LOG_CHANNEL {
            let msg = ExecutionLogMessage::deserialize(entry.serialized_body).map_err(|e| {
                ReplayError::MissingOrInvalidDescriptor(format!(
                    "failed to parse execution log message: {e}"
                ))
            })?;
            execution_log_entries.push(msg);
        } else {
            ordinary_log
                .entry((
                    entry.channel_name.to_owned(),
                    entry.header.published_at.to_nanoseconds(),
                ))
                .or_default()
                .push(entry.serialized_body.to_vec());
        }
    }

    // Check for dropped entries
    let total_dropped: usize = execution_log_entries
        .iter()
        .map(|m| m.number_of_dropped_entries.0)
        .sum();
    if total_dropped > 0 {
        return Err(ReplayError::DroppedExecutionLogEntries {
            count: total_dropped,
        });
    }

    // ── Phase 2: flatten into individual entries, group in insertion order ──
    //
    // Use a `Vec<ExecutionGroup>` plus an index map instead of a bare
    // `HashMap` so that groups preserve the order they were first seen in
    // the log.  Then sort by execution time with stable ordering for ties.

    let mut groups: Vec<ExecutionGroup> = Vec::new();
    let mut group_index: HashMap<(usize, i64), usize> = HashMap::new();

    for msg in &execution_log_entries {
        for entry in &msg.entries {
            if !entry.is_valid() {
                continue;
            }
            let key = (
                entry.callback_node_index as usize,
                entry.execution_time.to_nanoseconds(),
            );
            let idx = match group_index.get(&key) {
                Some(&i) => i,
                None => {
                    let i = groups.len();
                    groups.push(ExecutionGroup {
                        entries: Vec::new(),
                        key,
                    });
                    group_index.insert(key, i);
                    i
                }
            };
            groups[idx].entries.push(*entry);
        }
    }

    // Sort groups by execution time.  Use a stable sort so that groups with
    // equal timestamps preserve their original insertion order.
    groups.sort_by_key(|g| g.key.1);

    // ── Phase 3: resolve payloads in replay order ──────────────────────
    //
    // Now that groups are in deterministic order, iterate through them and
    // consume ordinary-log payloads FIFO per channel.

    let mut executions = Vec::new();
    let mut descriptor_less_executions = Vec::new();
    // Per-`(channel, published_at)` occurrence cursors, tracked independently
    // for received and published references so the producer and consumer of a
    // shared message each resolve to the same payload deterministically.
    let mut consumed: HashMap<(ChannelName, i64, bool), usize> = HashMap::new();

    for group in &groups {
        let node_idx = group.key.0;
        let execution_time = group.entries[0].execution_time;
        let execution_duration_ns = group.entries[0].execution_duration_ns;

        let Some(cd) = descriptor.index_to_callbacks.get(&node_idx) else {
            descriptor_less_executions.push((node_idx, execution_time));
            continue;
        };

        let mut received: HashMap<u16, Vec<(MessageHeader, Vec<u8>)>> = HashMap::new();
        let mut published: HashMap<u16, Vec<(MessageHeader, Vec<u8>)>> = HashMap::new();

        for entry in &group.entries {
            for msg in &entry.messages {
                if !msg.is_valid() {
                    break;
                }

                // Resolve the channel name from the descriptor.
                let (channel_name, is_received) =
                    if msg.direction == task::execution_log::Direction::Received {
                        let ch = cd
                            .subscriber_index_to_channel_name
                            .get(&(msg.ordinal as usize))
                            .ok_or_else(|| ReplayError::InvalidSubscriberOrdinal {
                                node: format!("node[{node_idx}]"),
                                ordinal: msg.ordinal,
                                subscriber_count: cd.subscriber_index_to_channel_name.len(),
                            })?;
                        (ch.clone(), true)
                    } else {
                        let ch = cd
                            .publisher_index_to_channel_name
                            .get(&(msg.ordinal as usize))
                            .ok_or_else(|| ReplayError::InvalidPublisherOrdinal {
                                node: format!("node[{node_idx}]"),
                                ordinal: msg.ordinal,
                                publisher_count: cd.publisher_index_to_channel_name.len(),
                            })?;
                        (ch.clone(), false)
                    };

                // Resolve the ordinary-log payload for this (channel, header).
                let body = lookup_payload(
                    &ordinary_log,
                    &mut consumed,
                    &channel_name,
                    &msg.header,
                    is_received,
                    node_idx,
                )?;

                if is_received {
                    received
                        .entry(msg.ordinal)
                        .or_default()
                        .push((msg.header, body));
                } else {
                    published
                        .entry(msg.ordinal)
                        .or_default()
                        .push((msg.header, body));
                }
            }
        }

        executions.push(ReplayExecution {
            callback_node_index: node_idx,
            execution_time,
            execution_duration_ns,
            received,
            published,
        });
    }

    Ok(ReplayLog {
        descriptor,
        executions,
        descriptor_less_executions,
    })
}

/// Resolve the ordinary-log payload for a `(channel, header_time)` reference.
///
/// The same physical message appears once in the ordinary log but is
/// referenced by both the producing execution (`Published`) and the consuming
/// execution (`Received`). Rather than consuming the entry, this indexes into
/// the ordered list of bodies for that `(channel, published_at)` key, using a
/// per-direction occurrence cursor. Unique timestamps resolve to the same
/// payload for both references; multiple same-timestamp messages are handed
/// out deterministically in log order.
fn lookup_payload(
    ordinary_log: &HashMap<(ChannelName, i64), Vec<Vec<u8>>>,
    consumed: &mut HashMap<(ChannelName, i64, bool), usize>,
    channel_name: &str,
    header: &MessageHeader,
    is_received: bool,
    node_idx: usize,
) -> Result<Vec<u8>, ReplayError> {
    let key = (
        channel_name.to_owned(),
        header.published_at.to_nanoseconds(),
    );
    let bodies = ordinary_log
        .get(&key)
        .ok_or_else(|| ReplayError::MissingOrdinaryPayload {
            channel: channel_name.to_owned(),
            header_time: format!("{:?}", header.published_at),
            direction: if is_received { "received" } else { "published" }.to_owned(),
            node: format!("node[{node_idx}]"),
        })?;

    let cursor = consumed
        .entry((
            channel_name.to_owned(),
            header.published_at.to_nanoseconds(),
            is_received,
        ))
        .or_insert(0);
    let body = bodies
        .get(*cursor)
        .ok_or_else(|| ReplayError::MissingOrdinaryPayload {
            channel: channel_name.to_owned(),
            header_time: format!("{:?}", header.published_at),
            direction: if is_received { "received" } else { "published" }.to_owned(),
            node: format!("node[{node_idx}]"),
        })?;
    *cursor += 1;
    Ok(body.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use logging::log_file::LogFileWriter;
    use logging::log_file_json::{JsonLogFileReader, JsonLogFileWriter};
    use task::time::FrameworkTime;

    /// Helper to write a JSON log entry to a buffer.
    fn write_entry(
        writer: &mut dyn LogFileWriter,
        channel: &str,
        header: &MessageHeader,
        body: &[u8],
    ) {
        writer.store_message(channel, header, body).unwrap();
    }

    /// Helper to write an artifact to a buffer.
    fn write_artifact(writer: &mut dyn LogFileWriter, name: &str, body: &[u8]) {
        writer.write_artifact(name, body).unwrap();
    }

    // `JsonLogFileWriter` borrows the backing buffer but has no Drop
    // implementation. Consume it explicitly so that the borrow ends without
    // triggering clippy::drop_non_drop.
    fn finish_writer<W: std::io::Write>(_: JsonLogFileWriter<W>) {}

    fn execution_log_bytes(entries: &[ExecutionLogEntry], dropped: usize) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "number_of_dropped_entries": dropped,
            "entries": entries,
        }))
        .unwrap()
    }

    #[test]
    fn parse_descriptor_only() {
        let mut buf = Vec::<u8>::new();
        let mut writer = logging::log_file_json::JsonLogFileWriter::new(&mut buf);

        let desc = ExecutionLogDescriptor::new(&[]);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        write_artifact(&mut writer, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes);
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let result = parse_replay_log(&reader);
        assert!(result.is_ok());
        let log = result.unwrap();
        assert!(log.executions.is_empty());
    }

    #[test]
    fn parse_rejects_dropped_entries() {
        let mut buf = Vec::<u8>::new();
        let mut writer = logging::log_file_json::JsonLogFileWriter::new(&mut buf);

        let desc = ExecutionLogDescriptor::new(&[]);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        write_artifact(&mut writer, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes);

        let scratch = execution_log_bytes(&[], 5);
        let header = MessageHeader::new(FrameworkTime::from_nanoseconds(0));
        write_entry(&mut writer, EXECUTION_LOG_CHANNEL, &header, &scratch);
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let result = parse_replay_log(&reader);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ReplayError::DroppedExecutionLogEntries { count: 5 }
        ));
    }

    #[test]
    fn parse_without_descriptor_fails() {
        let mut buf = Vec::<u8>::new();
        let mut writer = logging::log_file_json::JsonLogFileWriter::new(&mut buf);
        let header = MessageHeader::new(FrameworkTime::from_nanoseconds(0));
        write_entry(&mut writer, "some_other_channel", &header, b"hello");
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let result = parse_replay_log(&reader);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ReplayError::MissingOrInvalidDescriptor(_)
        ));
    }

    #[test]
    fn missing_payload_is_an_error() {
        let mut desc = ExecutionLogDescriptor::new(&[]);
        let mut sub_map = HashMap::new();
        sub_map.insert(0usize, "input".to_string());
        desc.index_to_callbacks.insert(
            0usize,
            task::execution_log::CallbackDescriptor {
                subscriber_index_to_channel_name: sub_map,
                publisher_index_to_channel_name: HashMap::new(),
            },
        );

        let mut buf = Vec::<u8>::new();
        let mut writer = logging::log_file_json::JsonLogFileWriter::new(&mut buf);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        write_artifact(&mut writer, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes);

        let mut entry = task::execution_log::ExecutionLogEntry {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(100),
            execution_duration_ns: 0,
            messages: std::array::from_fn(|_| task::execution_log::LoggedMessage::default()),
        };
        entry.messages[0] = task::execution_log::LoggedMessage {
            ordinal: 0,
            direction: task::execution_log::Direction::Received,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
        };
        let scratch = execution_log_bytes(&[entry], 0);
        write_entry(
            &mut writer,
            EXECUTION_LOG_CHANNEL,
            &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
            &scratch,
        );
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let result = parse_replay_log(&reader);
        assert!(result.is_err(), "missing ordinary payload must be an error");
        assert!(matches!(
            result.unwrap_err(),
            ReplayError::MissingOrdinaryPayload { .. }
        ));
    }

    #[test]
    fn descriptor_less_execution_recorded() {
        let desc = ExecutionLogDescriptor::new(&[]);
        let mut buf = Vec::<u8>::new();
        let mut writer = logging::log_file_json::JsonLogFileWriter::new(&mut buf);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        write_artifact(&mut writer, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes);

        let entry = task::execution_log::ExecutionLogEntry {
            callback_node_index: 5,
            execution_time: FrameworkTime::from_nanoseconds(100),
            execution_duration_ns: 0,
            messages: std::array::from_fn(|_| task::execution_log::LoggedMessage::default()),
        };
        let scratch = execution_log_bytes(&[entry], 0);
        write_entry(
            &mut writer,
            EXECUTION_LOG_CHANNEL,
            &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
            &scratch,
        );
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let log = parse_replay_log(&reader).expect("should parse");
        assert!(log.executions.is_empty());
        assert_eq!(log.descriptor_less_executions.len(), 1);
        assert_eq!(log.descriptor_less_executions[0].0, 5);
    }

    /// Verify deterministic FIFO payload matching when two execution-log
    /// entries reference the same channel and timestamp.
    #[test]
    fn deterministic_payload_matching() {
        use task::execution_log::Direction;
        use task::execution_log::LoggedMessage;

        let mut desc = ExecutionLogDescriptor::new(&[]);
        let mut sub_map = HashMap::new();
        sub_map.insert(0usize, "ch".to_string());
        desc.index_to_callbacks.insert(
            0usize,
            task::execution_log::CallbackDescriptor {
                subscriber_index_to_channel_name: sub_map,
                publisher_index_to_channel_name: HashMap::new(),
            },
        );

        let mut buf = Vec::<u8>::new();
        let mut writer = logging::log_file_json::JsonLogFileWriter::new(&mut buf);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        write_artifact(&mut writer, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes);

        // Two ordinary-log entries on "ch" at the same time.
        let hdr = MessageHeader::new(FrameworkTime::from_nanoseconds(100));
        write_entry(&mut writer, "ch", &hdr, b"first");
        write_entry(&mut writer, "ch", &hdr, b"second");

        // Two execution-log entries with different timestamps so they form
        // separate groups, both referencing ordinary-log payloads with the
        // same (channel, header_time).
        let bodies: &[&[u8]] = &[b"first", b"second"];
        for (i, _expected_body) in bodies.iter().enumerate() {
            let exec_time = FrameworkTime::from_nanoseconds(100 + i as i64);
            let mut entry = ExecutionLogEntry {
                callback_node_index: 0,
                execution_time: exec_time,
                execution_duration_ns: 0,
                messages: std::array::from_fn(|_| LoggedMessage::default()),
            };
            entry.messages[0] = LoggedMessage {
                ordinal: 0,
                direction: Direction::Received,
                header: hdr,
            };
            let scratch = execution_log_bytes(&[entry], 0);
            write_entry(
                &mut writer,
                EXECUTION_LOG_CHANNEL,
                &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
                &scratch,
            );
        }
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let log = parse_replay_log(&reader).expect("should parse");
        assert_eq!(log.executions.len(), 2);

        // The first execution should get "first", the second "second".
        let ex0 = &log.executions[0];
        let ex1 = &log.executions[1];
        let msgs0 = ex0.received.get(&0).unwrap();
        let msgs1 = ex1.received.get(&0).unwrap();
        assert_eq!(msgs0.len(), 1);
        assert_eq!(msgs1.len(), 1);
        assert_eq!(
            msgs0[0].1, b"first",
            "first execution should get the first ordinary-log entry"
        );
        assert_eq!(
            msgs1[0].1, b"second",
            "second execution should get the second ordinary-log entry"
        );
    }
}
