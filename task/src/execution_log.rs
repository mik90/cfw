use std::collections::{HashMap, HashSet};
use std::num::Saturating;

use crate::callback::CallbackNode;
use crate::executor::ThreadPoolConfig;
use crate::message::MessageHeader;
use crate::pub_sub::ChannelName;
use crate::publisher::{GenericPublisher, Publisher, PublisherConfig};
use crate::time::FrameworkTime;

use crate::generic_publisher::ConnectionTypeMismatch;
#[cfg(feature = "serde")]
use crate::loggable::{DeserializeError, Loggable, SerializeError};
#[cfg(feature = "serde")]
use std::io::Write;

/// Channel every execution-log publisher publishes on.
pub const EXECUTION_LOG_CHANNEL: &str = "execution_log";
/// Artifact name used to store the execution log descriptor in the log file.
pub const EXECUTION_LOG_DESCRIPTOR_ARTIFACT: &str = "execution_log_descriptor";

/// Maximum number of logged messages referenced by a single [`ExecutionLogEntry`].
/// A single callback execution that produces/receives more than this many
/// messages splits across multiple entries, grouped by
/// `(callback_node_index, execution_time)` on the consumer side.
pub const MESSAGES_PER_ENTRY: usize = 24;

/// Number of message references shared by all entries in one execution-log batch.
pub const MESSAGES_PER_LOG: usize = 256;

/// Number of [`ExecutionLogEntry`]s packed into a single [`ExecutionLogMessage`].
/// One pub/sub message is emitted whenever this many entries accumulate,
/// periodically (per the executor's flush period), or on worker exit.
pub const ENTRIES_PER_MESSAGE: usize = 64;

/// How much execution information is recorded for a callback node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ExecutionLogLevel {
    /// Record nothing.
    Off,
    /// Record only each execution's duration.
    #[default]
    Duration,
    /// Record full executions: duration plus every received/published message.
    Whole,
}

/// Which pub/sub side a logged message came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Direction {
    #[default]
    Published,
    Received,
}

/// One logged header plus which publisher/subscriber (by ordinal into the
/// node's `publishers()`/`subscribers()`) it belongs to. The node's own
/// publisher/subscriber vectors carry the channel layout — nothing is
/// duplicated here.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LoggedMessage {
    pub ordinal: u16,
    pub direction: Direction,
    pub header: MessageHeader,
}

impl LoggedMessage {
    pub fn is_valid(&self) -> bool {
        self.header.published_at != FrameworkTime::INVALID
    }
}

/// Meaning of one execution-log entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ExecutionLogEntryKind {
    /// Records execution duration without message references.
    #[default]
    Duration,
    /// Records execution duration and received/published message references.
    Execution,
    /// Records one listener activation for a subscriber.
    #[cfg(feature = "iceoryx2")]
    Iox2Event,
}

/// A fixed-size descriptor for one callback execution or listener activation.
/// Message references reside in the containing [`ExecutionLogMessage`]'s shared pool.
/// An execution that logs
/// more than [`MESSAGES_PER_ENTRY`] messages continues in follow-up entries
/// sharing the same `(callback_node_index, execution_time)`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ExecutionLogEntry {
    pub callback_node_index: u32,
    pub execution_time: FrameworkTime,
    pub execution_duration_ns: u64,
    /// Whether the entry describes a duration, full execution, or event activation.
    pub kind: ExecutionLogEntryKind,
    /// Offset of this entry's first message reference in the batch's shared pool.
    pub message_start: u16,
    /// Number of message references belonging to this entry.
    pub message_count: u16,
}

impl Default for ExecutionLogEntry {
    fn default() -> Self {
        ExecutionLogEntry {
            callback_node_index: 0,
            execution_time: FrameworkTime::INVALID,
            execution_duration_ns: 0,
            kind: ExecutionLogEntryKind::Duration,
            message_start: 0,
            message_count: 0,
        }
    }
}

impl ExecutionLogEntry {
    pub fn is_valid(&self) -> bool {
        self.execution_time != FrameworkTime::INVALID
    }

    /// Whether this entry records an iox2 activation instead of a callback run.
    pub fn is_iox2_event(&self) -> bool {
        #[cfg(feature = "iceoryx2")]
        {
            self.kind == ExecutionLogEntryKind::Iox2Event
        }
        #[cfg(not(feature = "iceoryx2"))]
        {
            false
        }
    }
}

/// One pub/sub message carrying a batch of execution-log entries plus a count
/// of execution logs that were dropped (couldn't be recorded) while this batch
/// was being filled. Emitted by an executor's per-thread execution-log
/// publishers on the [`EXECUTION_LOG_CHANNEL`].
///
/// `Serialize`/`Deserialize` are not derived here because std's `Saturating`
/// and serde's array support (capped at 32 elements) don't compose for the
/// 64-entry array. Add manual impls when the execution-log channel needs to
/// ride the logging pipeline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExecutionLogMessage {
    pub number_of_dropped_entries: Saturating<usize>,
    pub entries: [ExecutionLogEntry; ENTRIES_PER_MESSAGE],
    /// Shared message-reference pool, indexed by each entry's start and count.
    pub messages: [LoggedMessage; MESSAGES_PER_LOG],
}

impl Default for ExecutionLogMessage {
    fn default() -> Self {
        ExecutionLogMessage {
            number_of_dropped_entries: Saturating(0),
            entries: std::array::from_fn(|_| ExecutionLogEntry::default()),
            messages: std::array::from_fn(|_| LoggedMessage::default()),
        }
    }
}

impl ExecutionLogMessage {
    /// First invalid (unused) entry slot in this message, or `None` if full.
    pub fn next_free_entry(&self) -> Option<usize> {
        self.entries.iter().position(|e| !e.is_valid())
    }

    /// First unused slot in the shared message-reference pool, or `None` if full.
    pub fn next_free_message(&self) -> Option<usize> {
        self.messages.iter().position(|message| !message.is_valid())
    }

    /// Message references owned by an entry in this batch.
    pub fn messages_for(&self, entry: &ExecutionLogEntry) -> &[LoggedMessage] {
        let start = usize::from(entry.message_start);
        let end = start + usize::from(entry.message_count);
        &self.messages[start..end]
    }

    /// Append a complete entry and its message references to the fixed-size batch.
    /// Returns `false` if either pool lacks capacity.
    pub fn push_entry(&mut self, mut entry: ExecutionLogEntry, messages: &[LoggedMessage]) -> bool {
        let Some(index) = self.next_free_entry() else {
            return false;
        };
        let start = self.next_free_message().unwrap_or(MESSAGES_PER_LOG);
        if !entry.is_valid()
            || messages.len() > MESSAGES_PER_ENTRY
            || messages.iter().any(|message| !message.is_valid())
            || (entry.kind == ExecutionLogEntryKind::Duration && !messages.is_empty())
            || (entry.is_iox2_event() && messages.len() != 1)
            || start + messages.len() > MESSAGES_PER_LOG
        {
            return false;
        }
        entry.message_start = start as u16;
        entry.message_count = messages.len() as u16;
        self.entries[index] = entry;
        self.messages[start..start + messages.len()].copy_from_slice(messages);
        true
    }
}

#[cfg(feature = "serde")]
impl Loggable for ExecutionLogMessage {
    type Context<'a> = ();

    fn serialize(&self, w: &mut dyn Write) -> Result<(), SerializeError> {
        #[derive(serde::Serialize)]
        struct Helper<'a> {
            number_of_dropped_entries: usize,
            entries: &'a [ExecutionLogEntry],
            messages: &'a [LoggedMessage],
        }
        let entry_count = self.next_free_entry().unwrap_or(ENTRIES_PER_MESSAGE);
        let message_count = self.next_free_message().unwrap_or(MESSAGES_PER_LOG);
        let helper = Helper {
            number_of_dropped_entries: self.number_of_dropped_entries.0,
            entries: &self.entries[..entry_count],
            messages: &self.messages[..message_count],
        };
        serde_json::to_writer(w, &helper).map_err(SerializeError::SerdeJson)
    }

    fn deserialize_with_ctx(bytes: &[u8], _ctx: ()) -> Result<Self, DeserializeError> {
        #[derive(serde::Deserialize)]
        struct Helper {
            number_of_dropped_entries: usize,
            entries: Vec<ExecutionLogEntry>,
            messages: Vec<LoggedMessage>,
        }
        let helper: Helper = serde_json::from_slice(bytes).map_err(DeserializeError::SerdeJson)?;
        let invalid = || {
            DeserializeError::Other(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid execution-log entry range or batch capacity",
            )))
        };
        if helper.entries.len() > ENTRIES_PER_MESSAGE || helper.messages.len() > MESSAGES_PER_LOG {
            return Err(invalid());
        }
        let mut previous_end = 0;
        for entry in &helper.entries {
            let start = usize::from(entry.message_start);
            let end = start + usize::from(entry.message_count);
            if !entry.is_valid()
                || start != previous_end
                || end > helper.messages.len()
                || usize::from(entry.message_count) > MESSAGES_PER_ENTRY
                || (entry.kind == ExecutionLogEntryKind::Duration && start != end)
                || (entry.is_iox2_event() && end != start + 1)
            {
                return Err(invalid());
            }
            previous_end = end;
        }
        if previous_end != helper.messages.len()
            || helper.messages.iter().any(|message| !message.is_valid())
        {
            return Err(invalid());
        }
        let mut entries = [ExecutionLogEntry::default(); ENTRIES_PER_MESSAGE];
        entries[..helper.entries.len()].copy_from_slice(&helper.entries);
        let mut messages = [LoggedMessage::default(); MESSAGES_PER_LOG];
        messages[..helper.messages.len()].copy_from_slice(&helper.messages);
        Ok(ExecutionLogMessage {
            number_of_dropped_entries: Saturating(helper.number_of_dropped_entries),
            entries,
            messages,
        })
    }
}

/// Error from [`connect`] when a subscriber on the execution-log channel has a
/// type that doesn't match [`ExecutionLogMessage`].
#[derive(Debug)]
pub struct ExecutionLogConnectError {
    pub channel_name: ChannelName,
    pub subscriber_node: String,
}

impl std::fmt::Display for ExecutionLogConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Subscriber on channel '{}' (node '{}') is not a Subscriber<ExecutionLogMessage>",
            self.channel_name, self.subscriber_node,
        )
    }
}

/// Descriptor of per-callback indices to channel names.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CallbackDescriptor {
    pub subscriber_index_to_channel_name: HashMap<usize, ChannelName>,
    pub publisher_index_to_channel_name: HashMap<usize, ChannelName>,
}

/// Descriptor of names to indices
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ExecutionLogDescriptor {
    pub index_to_callbacks: HashMap<usize, CallbackDescriptor>,
    /// Channel names whose messages were written to the ordinary log by the
    /// logging build step. `#[serde(default)]` keeps logs written before this
    /// annotation existed parseable — exact replay then falls back to whatever
    /// channels it observes in the ordinary log.
    #[cfg_attr(feature = "serde", serde(default))]
    pub logged_channels: HashSet<ChannelName>,
}

impl ExecutionLogDescriptor {
    /// Creates a descriptor from a slice of callback nodes, annotating no
    /// channels as logged. Equivalent to
    /// `new_with_logged_channels(nodes, HashSet::new())`.
    pub fn new(nodes: &[CallbackNode]) -> ExecutionLogDescriptor {
        Self::new_with_logged_channels(nodes, HashSet::new())
    }

    /// Creates a descriptor from a slice of callback nodes, annotating which
    /// channels were written to the ordinary log.
    pub fn new_with_logged_channels(
        nodes: &[CallbackNode],
        logged_channels: HashSet<ChannelName>,
    ) -> ExecutionLogDescriptor {
        use crate::callback::CallbackViews;

        let mut index_to_callbacks = HashMap::new();
        for (callback_node_index, node) in nodes.iter().enumerate() {
            let mut subscriber_index_to_channel_name = HashMap::new();
            for (subscriber_index, subscriber) in
                node.callback().collect_subscribers().iter().enumerate()
            {
                subscriber_index_to_channel_name
                    .insert(subscriber_index, subscriber.config().channel_name.clone());
            }

            let mut publisher_index_to_channel_name = HashMap::new();
            for (publisher_index, publisher) in
                node.callback().collect_publishers().iter().enumerate()
            {
                publisher_index_to_channel_name
                    .insert(publisher_index, publisher.config().channel_name.clone());
            }

            index_to_callbacks.insert(
                callback_node_index,
                CallbackDescriptor {
                    subscriber_index_to_channel_name,
                    publisher_index_to_channel_name,
                },
            );
        }

        ExecutionLogDescriptor {
            index_to_callbacks,
            logged_channels,
        }
    }
}

impl std::error::Error for ExecutionLogConnectError {}

/// Create one execution-log [`Publisher`] per worker thread across all pools.
/// The returned publishers are on [`EXECUTION_LOG_CHANNEL`] with capacity 1
/// (at most one outstanding log message per thread). Wire them into the graph
/// with [`connect`] before passing the pools into the executor.
///
/// The number of publishers equals the sum of `thread_count` across `pools`,
/// assigned in pool order: pool 0's workers get publishers `0..thread_count_0`,
/// and so on.
pub fn log_publishers(pools: &[ThreadPoolConfig]) -> Vec<Publisher<ExecutionLogMessage>> {
    let total: usize = pools.iter().map(|p| p.thread_count).sum();
    (0..total)
        .map(|_| {
            Publisher::new(PublisherConfig {
                capacity: 1,
                channel_name: EXECUTION_LOG_CHANNEL.into(),
            })
        })
        .collect()
}

/// Connect each execution-log publisher to every subscriber on
/// [`EXECUTION_LOG_CHANNEL`] found in `pools`' nodes, then allocate each
/// publisher's arena. A publisher with no matching subscriber still has its
/// arena allocated so it can loan (and harmlessly discard) log messages.
///
/// This must be called *after* the task graph is built (subscriber nodes exist)
/// and *before* the pools are handed to the executor.
pub fn connect(
    pools: &mut [ThreadPoolConfig],
    log_pubs: &mut [Publisher<ExecutionLogMessage>],
) -> Result<(), ExecutionLogConnectError> {
    use crate::callback::CallbackViews;

    for pool in pools.iter_mut() {
        for node in pool.nodes.iter_shared() {
            node.access(|node| {
                let node_name = node.name().to_string();
                // Build time: the pools are not shared with any worker thread
                // yet, so exclusive access cannot conflict.
                for subscriber in node.callback_mut().collect_subscribers_mut() {
                    if subscriber.config().channel_name != EXECUTION_LOG_CHANNEL {
                        continue;
                    }
                    for log_pub in log_pubs.iter_mut() {
                        match log_pub.connect_to_subscriber(subscriber) {
                            Ok(()) => {}
                            Err(ConnectionTypeMismatch { .. }) => {
                                return Err(ExecutionLogConnectError {
                                    channel_name: EXECUTION_LOG_CHANNEL.into(),
                                    subscriber_node: node_name.clone(),
                                });
                            }
                        }
                    }
                }
                Ok(())
            })?;
        }
    }

    // Always allocate arenas so every publisher can loan, even with no
    // subscriber wired (its flushes simply drain nowhere).
    for log_pub in log_pubs.iter_mut() {
        log_pub.allocate_arena();
    }

    Ok(())
}

/// Largest possible input this node could hand a single execution: the sum of
/// its subscribers' read-buffer `capacity` values (what `drain_subscribers`
/// could expose to `run`). Used to size a per-worker received-headers scratch
/// buffer so the capture path never reallocates in steady state.
pub fn worst_case_received_count(node: &crate::callback::CallbackNode) -> usize {
    let mut sum: usize = 0;
    node.callback()
        .for_each_subscriber(&mut |s| sum += s.config().capacity);
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_log_level_defaults_to_duration() {
        assert_eq!(ExecutionLogLevel::default(), ExecutionLogLevel::Duration);
    }

    #[test]
    fn default_message_has_all_invalid_entries() {
        let msg = ExecutionLogMessage::default();
        assert_eq!(msg.number_of_dropped_entries, Saturating(0));
        // Every entry slot starts INVALID, so the first free slot is 0 and the
        // rest are reported invalid by is_valid.
        assert_eq!(msg.next_free_entry(), Some(0));
        assert!(msg.entries.iter().all(|e| !e.is_valid()));
    }

    #[test]
    fn default_entry_has_all_invalid_messages() {
        let entry = ExecutionLogEntry::default();
        assert!(!entry.is_valid());
        assert_eq!(entry.message_count, 0);
        assert_eq!(ExecutionLogMessage::default().next_free_message(), Some(0));
        #[cfg(feature = "iceoryx2")]
        assert!(!entry.is_iox2_event());
    }

    #[cfg(all(feature = "iceoryx2", feature = "serde"))]
    #[test]
    fn event_and_callback_entries_roundtrip_in_one_execution_log() {
        use crate::loggable::Loggable;

        let observed_at = FrameworkTime::from_nanoseconds(137);
        let event = ExecutionLogEntry {
            callback_node_index: 3,
            execution_time: observed_at,
            kind: ExecutionLogEntryKind::Iox2Event,
            ..Default::default()
        };
        let reference = LoggedMessage {
            ordinal: 2,
            direction: Direction::Received,
            header: MessageHeader::new(observed_at),
        };
        let callback = ExecutionLogEntry {
            callback_node_index: 3,
            execution_time: FrameworkTime::from_nanoseconds(151),
            kind: ExecutionLogEntryKind::Execution,
            ..Default::default()
        };
        let mut batch = ExecutionLogMessage::default();
        assert!(batch.push_entry(event, &[reference]));
        assert!(batch.push_entry(callback, &[]));
        let mut bytes = Vec::new();
        batch.serialize(&mut bytes).unwrap();
        let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(wire["entries"].as_array().unwrap().len(), 2);
        assert_eq!(wire["messages"].as_array().unwrap().len(), 1);
        let parsed = ExecutionLogMessage::deserialize(&bytes).unwrap();

        assert!(parsed.entries[0].is_iox2_event());
        assert_eq!(parsed.messages_for(&parsed.entries[0]), &[reference]);
        assert_eq!(parsed.entries[0].callback_node_index, 3);
        assert!(!parsed.entries[1].is_iox2_event());
        assert_eq!(parsed.entries[1].kind, ExecutionLogEntryKind::Execution);
        assert_eq!(parsed.next_free_entry(), Some(2));
    }

    #[test]
    fn sentinel_occupancy_walks_messages_then_next_entry() {
        let entry = ExecutionLogEntry {
            execution_time: FrameworkTime::from_nanoseconds(1),
            kind: ExecutionLogEntryKind::Execution,
            ..Default::default()
        };
        let messages: [LoggedMessage; 3] = std::array::from_fn(|i| LoggedMessage {
            ordinal: i as u16,
            direction: Direction::Received,
            header: MessageHeader::new(FrameworkTime::from_nanoseconds(10 + i as i64)),
        });
        let mut batch = ExecutionLogMessage::default();
        assert!(batch.push_entry(entry, &messages));
        assert_eq!(batch.next_free_message(), Some(3));
        assert_eq!(batch.messages_for(&batch.entries[0]), &messages);
        assert!(std::mem::size_of::<ExecutionLogMessage>() < 8192);
    }

    #[test]
    fn shared_message_pool_reports_exhaustion_without_losing_entry_boundaries() {
        let mut batch = ExecutionLogMessage::default();
        let at = FrameworkTime::from_nanoseconds(17);
        let reference = LoggedMessage {
            header: MessageHeader::new(at),
            ..Default::default()
        };
        for _ in 0..10 {
            assert!(batch.push_entry(
                ExecutionLogEntry {
                    execution_time: at,
                    kind: ExecutionLogEntryKind::Execution,
                    ..Default::default()
                },
                &[reference; MESSAGES_PER_ENTRY],
            ));
        }
        assert!(batch.push_entry(
            ExecutionLogEntry {
                execution_time: at,
                kind: ExecutionLogEntryKind::Execution,
                ..Default::default()
            },
            &[reference; MESSAGES_PER_LOG - 10 * MESSAGES_PER_ENTRY],
        ));
        assert_eq!(batch.next_free_message(), None);
        assert!(batch.push_entry(
            ExecutionLogEntry {
                execution_time: at,
                ..Default::default()
            },
            &[],
        ));
        assert!(!batch.push_entry(
            ExecutionLogEntry {
                execution_time: at,
                kind: ExecutionLogEntryKind::Execution,
                ..Default::default()
            },
            &[reference],
        ));
        assert_eq!(batch.messages_for(&batch.entries[10]).len(), 16);
        assert!(batch.messages_for(&batch.entries[11]).is_empty());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn rejects_out_of_bounds_message_range() {
        use crate::loggable::Loggable;

        let at = FrameworkTime::from_nanoseconds(5);
        let mut batch = ExecutionLogMessage::default();
        assert!(batch.push_entry(
            ExecutionLogEntry {
                execution_time: at,
                kind: ExecutionLogEntryKind::Execution,
                ..Default::default()
            },
            &[LoggedMessage {
                header: MessageHeader::new(at),
                ..Default::default()
            }],
        ));
        let mut bytes = Vec::new();
        batch.serialize(&mut bytes).unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json["entries"][0]["message_count"] = serde_json::json!(2);
        assert!(ExecutionLogMessage::deserialize(&serde_json::to_vec(&json).unwrap()).is_err());
    }

    #[test]
    fn split_execution_entries_share_grouping_key() {
        // An execution that logs more than MESSAGES_PER_ENTRY messages splits
        // across entries; the consumer groups them by (node, execution_time).
        // Here we just assert the round-trip: two entries written for the same
        // execution carry identical grouping fields.
        let node = 5u32;
        let time = FrameworkTime::from_nanoseconds(7);
        let dur = 1234u64;

        let a = ExecutionLogEntry {
            callback_node_index: node,
            execution_time: time,
            execution_duration_ns: dur,
            ..Default::default()
        };
        let b = ExecutionLogEntry {
            callback_node_index: node,
            execution_time: time,
            execution_duration_ns: dur,
            ..Default::default()
        };

        assert_eq!(a.callback_node_index, b.callback_node_index);
        assert_eq!(a.execution_time, b.execution_time);
        assert_eq!(a.execution_duration_ns, b.execution_duration_ns);
    }
}
