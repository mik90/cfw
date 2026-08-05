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

use std::collections::{HashMap, HashSet};

use logging::log_file::LogFileReader;
use task::execution_log::{
    Direction, EXECUTION_LOG_CHANNEL, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, ExecutionLogDescriptor,
    ExecutionLogEntry, ExecutionLogMessage,
};
use task::loggable::Loggable;
use task::message::MessageHeader;
use task::pub_sub::ChannelName;
use task::time::FrameworkTime;

use crate::error::ReplayError;

/// Where a message reference's payload comes from during replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PayloadSource {
    /// The payload was found in the ordinary log (the channel was logged).
    Logged(Vec<u8>),
    /// The payload was not logged; replay reproduces it by re-running the
    /// producing node and feeding its captured output to consumers.
    Reproduce,
    /// The payload was not logged and cannot be reproduced.
    Gap,
}

/// A single replayed execution: the callback node index, the time it executed,
/// the received messages (by subscriber ordinal), and the published messages
/// (by publisher ordinal). Each message reference carries its [`PayloadSource`]
/// — logged payloads are resolved at parse time, reproduced payloads are
/// recovered from the reproduction store during replay.
#[derive(Debug, Clone)]
pub(crate) struct ReplayExecution {
    pub callback_node_index: usize,
    pub execution_time: FrameworkTime,
    pub execution_duration_ns: u64,
    /// Messages received by each subscriber ordinal during this execution.
    /// Key: subscriber ordinal, Value: (header, payload source)
    pub received: HashMap<u16, Vec<(MessageHeader, PayloadSource)>>,
    /// Messages published by each publisher ordinal during this execution.
    /// Key: publisher ordinal, Value: (header, payload source)
    pub published: HashMap<u16, Vec<(MessageHeader, PayloadSource)>>,
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
    /// Ordinary-log payloads retained per channel, in log order. Replay needs
    /// these to build the [`ReplayMessageLog`] context that resolves the
    /// payload a forwarded message references by header. Messages on the
    /// execution-log channel are excluded (nothing forwards them).
    ///
    /// [`ReplayMessageLog`]: task::loggable::ReplayMessageLog
    pub source_messages: HashMap<ChannelName, Vec<(MessageHeader, Vec<u8>)>>,
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
    // Ordinary-log payloads retained per channel for forwarded-message context
    // resolution (see [`ReplayLog::source_messages`]).
    let mut source_messages: HashMap<ChannelName, Vec<(MessageHeader, Vec<u8>)>> = HashMap::new();

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
            source_messages
                .entry(entry.channel_name.to_owned())
                .or_default()
                .push((entry.header, entry.serialized_body.to_vec()));
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

    // The effective set of logged channels: whatever the descriptor annotated
    // plus whatever we actually observe in the ordinary log (backward
    // compatible with logs written before the annotation existed).
    let mut logged_channels: HashSet<ChannelName> = descriptor.logged_channels.clone();
    logged_channels.extend(ordinary_log.keys().map(|(channel, _)| channel.clone()));

    // Channels some node in the replay graph produces. A message on a channel
    // outside this set cannot be reproduced by re-running a producer.
    let reproducible_channels: HashSet<ChannelName> = descriptor
        .index_to_callbacks
        .values()
        .flat_map(|cd| cd.publisher_index_to_channel_name.values().cloned())
        .collect();

    let mut groups: Vec<ExecutionGroup> = Vec::new();
    let mut group_index: HashMap<(usize, i64), usize> = HashMap::new();

    for msg in &execution_log_entries {
        for entry in &msg.entries {
            if !entry.is_valid() {
                continue;
            }
            // Duration-only entries carry no messages and are treated as if
            // there was no execution log at all.
            if !entry.log_whole {
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

        let mut received: HashMap<u16, Vec<(MessageHeader, PayloadSource)>> = HashMap::new();
        let mut published: HashMap<u16, Vec<(MessageHeader, PayloadSource)>> = HashMap::new();

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
                // A channel that was logged must always resolve; a channel that
                // was not logged falls back to reproduction.
                let source = match lookup_payload(
                    &ordinary_log,
                    &mut consumed,
                    &channel_name,
                    &msg.header,
                    is_received,
                    node_idx,
                )? {
                    Some(body) => PayloadSource::Logged(body),
                    None => {
                        if logged_channels.contains(&channel_name)
                            || !reproducible_channels.contains(&channel_name)
                        {
                            return Err(ReplayError::UnreproducibleMessage {
                                channel: channel_name.clone(),
                                header_time: msg.header.published_at,
                                direction: if is_received {
                                    Direction::Received
                                } else {
                                    Direction::Published
                                },
                                node: format!("node[{node_idx}]"),
                                reason: if logged_channels.contains(&channel_name) {
                                    "channel was logged but no ordinary-log payload was recorded"
                                        .to_owned()
                                } else {
                                    "channel was not logged and has no producer in the replay \
                                     graph"
                                        .to_owned()
                                },
                            });
                        }
                        PayloadSource::Reproduce
                    }
                };

                if is_received {
                    received
                        .entry(msg.ordinal)
                        .or_default()
                        .push((msg.header, source));
                } else {
                    published
                        .entry(msg.ordinal)
                        .or_default()
                        .push((msg.header, source));
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
        source_messages,
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
///
/// Returns `Ok(None)` when the channel has no entries at all for that
/// timestamp — a channel that was not logged. A logged channel that runs out
/// of bodies is an error (the ordinary log is incomplete).
fn lookup_payload(
    ordinary_log: &HashMap<(ChannelName, i64), Vec<Vec<u8>>>,
    consumed: &mut HashMap<(ChannelName, i64, bool), usize>,
    channel_name: &str,
    header: &MessageHeader,
    is_received: bool,
    node_idx: usize,
) -> Result<Option<Vec<u8>>, ReplayError> {
    let key = (
        channel_name.to_owned(),
        header.published_at.to_nanoseconds(),
    );
    let Some(bodies) = ordinary_log.get(&key) else {
        return Ok(None);
    };

    let cursor = consumed
        .entry((
            channel_name.to_owned(),
            header.published_at.to_nanoseconds(),
            is_received,
        ))
        .or_insert(0);
    let body = bodies
        .get(*cursor)
        .ok_or_else(|| ReplayError::UnreproducibleMessage {
            channel: channel_name.to_owned(),
            header_time: header.published_at,
            direction: if is_received {
                Direction::Received
            } else {
                Direction::Published
            },
            node: format!("node[{node_idx}]"),
            reason: "logged channel is missing an ordinary-log payload".to_owned(),
        })?;
    *cursor += 1;
    Ok(Some(body.clone()))
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
            log_whole: true,
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
        assert!(
            result.is_err(),
            "an unreproducible payload must be an error"
        );
        assert!(matches!(
            result.unwrap_err(),
            ReplayError::UnreproducibleMessage { .. }
        ));
    }

    /// A message on a channel that is neither logged nor produced by any node
    /// in the graph is unreproducible and must be an error.
    #[test]
    fn unreproducible_message_is_an_error() {
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
            log_whole: true,
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
        assert!(result.is_err(), "unproducible channel must be an error");
        assert!(matches!(
            result.unwrap_err(),
            ReplayError::UnreproducibleMessage { .. }
        ));
    }

    /// A message on a channel that is not logged but IS produced by a node in
    /// the graph is classified as [`PayloadSource::Reproduce`] so replay can
    /// recover it by re-running the producer.
    #[test]
    fn unlogged_producible_channel_is_reproduced() {
        let mut desc = ExecutionLogDescriptor::new(&[]);
        // Node 0 publishes "source" (unlogged), node 1 subscribes to it.
        let mut pub_map = HashMap::new();
        pub_map.insert(0usize, "source".to_string());
        desc.index_to_callbacks.insert(
            0usize,
            task::execution_log::CallbackDescriptor {
                subscriber_index_to_channel_name: HashMap::new(),
                publisher_index_to_channel_name: pub_map,
            },
        );
        let mut sub_map = HashMap::new();
        sub_map.insert(0usize, "source".to_string());
        desc.index_to_callbacks.insert(
            1usize,
            task::execution_log::CallbackDescriptor {
                subscriber_index_to_channel_name: sub_map,
                publisher_index_to_channel_name: HashMap::new(),
            },
        );

        let mut buf = Vec::<u8>::new();
        let mut writer = logging::log_file_json::JsonLogFileWriter::new(&mut buf);
        let desc_bytes = serde_json::to_vec(&desc).unwrap();
        write_artifact(&mut writer, EXECUTION_LOG_DESCRIPTOR_ARTIFACT, &desc_bytes);

        // Node 0 published "source" at 100; node 1 received it at 100. No
        // ordinary-log entry for "source" — it was not logged.
        let mut producer = task::execution_log::ExecutionLogEntry {
            callback_node_index: 0,
            execution_time: FrameworkTime::from_nanoseconds(100),
            execution_duration_ns: 0,
            log_whole: true,
            messages: std::array::from_fn(|_| task::execution_log::LoggedMessage::default()),
        };
        producer.messages[0] = task::execution_log::LoggedMessage {
            ordinal: 0,
            direction: task::execution_log::Direction::Published,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
        };
        let mut consumer = task::execution_log::ExecutionLogEntry {
            callback_node_index: 1,
            execution_time: FrameworkTime::from_nanoseconds(150),
            execution_duration_ns: 0,
            log_whole: true,
            messages: std::array::from_fn(|_| task::execution_log::LoggedMessage::default()),
        };
        consumer.messages[0] = task::execution_log::LoggedMessage {
            ordinal: 0,
            direction: task::execution_log::Direction::Received,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(100)),
        };
        let scratch = execution_log_bytes(&[producer, consumer], 0);
        write_entry(
            &mut writer,
            EXECUTION_LOG_CHANNEL,
            &MessageHeader::new(FrameworkTime::from_nanoseconds(0)),
            &scratch,
        );
        finish_writer(writer);

        let reader = JsonLogFileReader::from_reader(buf.as_slice()).unwrap();
        let log = parse_replay_log(&reader).expect("should parse");
        assert_eq!(log.executions.len(), 2);

        let produced = log.executions[0].published.get(&0).unwrap();
        let received = log.executions[1].received.get(&0).unwrap();
        assert!(matches!(produced[0].1, PayloadSource::Reproduce));
        assert!(matches!(received[0].1, PayloadSource::Reproduce));
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
            log_whole: true,
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
                log_whole: true,
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
            msgs0[0].1,
            PayloadSource::Logged(b"first".to_vec()),
            "first execution should get the first ordinary-log entry"
        );
        assert_eq!(
            msgs1[0].1,
            PayloadSource::Logged(b"second".to_vec()),
            "second execution should get the second ordinary-log entry"
        );
    }
}
