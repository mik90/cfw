use std::collections::HashMap;
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
/// Descriptor that describes the index->channel mapping
pub const EXECUTION_LOG_DESCRIPTOR_CHANNEL: &str = "execution_log_descriptor";

/// Number of logged messages packed into a single [`ExecutionLogEntry`].
/// A single callback execution that produces/receives more than this many
/// messages splits across multiple entries, grouped by
/// `(callback_node_index, execution_time)` on the consumer side.
pub const MESSAGES_PER_ENTRY: usize = 24;

/// Number of [`ExecutionLogEntry`]s packed into a single [`ExecutionLogMessage`].
/// One pub/sub message is emitted whenever this many entries accumulate,
/// periodically (per the executor's flush period), or on worker exit.
pub const ENTRIES_PER_MESSAGE: usize = 64;

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

/// A fixed-size slice of one callback's execution. An execution that logs
/// more than [`MESSAGES_PER_ENTRY`] messages continues in follow-up entries
/// sharing the same `(callback_node_index, execution_time)`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ExecutionLogEntry {
    pub callback_node_index: u32,
    pub execution_time: FrameworkTime,
    pub execution_duration_ns: u64,
    pub messages: [LoggedMessage; MESSAGES_PER_ENTRY],
}

impl Default for ExecutionLogEntry {
    fn default() -> Self {
        ExecutionLogEntry {
            callback_node_index: 0,
            execution_time: FrameworkTime::INVALID,
            execution_duration_ns: 0,
            messages: std::array::from_fn(|_| LoggedMessage::default()),
        }
    }
}

impl ExecutionLogEntry {
    pub fn is_valid(&self) -> bool {
        self.execution_time != FrameworkTime::INVALID
    }

    /// First invalid (unused) message slot in this entry, or `None` if full.
    pub fn next_free(&self) -> Option<usize> {
        self.messages.iter().position(|m| !m.is_valid())
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
}

impl Default for ExecutionLogMessage {
    fn default() -> Self {
        ExecutionLogMessage {
            number_of_dropped_entries: Saturating(0),
            entries: std::array::from_fn(|_| ExecutionLogEntry::default()),
        }
    }
}

impl ExecutionLogMessage {
    /// First invalid (unused) entry slot in this message, or `None` if full.
    pub fn next_free_entry(&self) -> Option<usize> {
        self.entries.iter().position(|e| !e.is_valid())
    }
}

#[cfg(feature = "serde")]
impl<'a> Loggable<'a> for ExecutionLogMessage {
    type Context = ();

    fn serialize(&self, w: &mut dyn Write) -> Result<(), SerializeError> {
        #[derive(serde::Serialize)]
        struct Helper<'a> {
            number_of_dropped_entries: usize,
            entries: &'a [ExecutionLogEntry],
        }
        let helper = Helper {
            number_of_dropped_entries: self.number_of_dropped_entries.0,
            entries: &self.entries,
        };
        serde_json::to_writer(w, &helper).map_err(SerializeError::SerdeJson)
    }

    fn deserialize_with_ctx(bytes: &[u8], _ctx: ()) -> Result<Self, DeserializeError> {
        #[derive(serde::Deserialize)]
        struct Helper {
            number_of_dropped_entries: usize,
            entries: Vec<ExecutionLogEntry>,
        }
        let helper: Helper = serde_json::from_slice(bytes).map_err(DeserializeError::SerdeJson)?;
        let mut entries = [ExecutionLogEntry::default(); ENTRIES_PER_MESSAGE];
        let len = helper.entries.len().min(ENTRIES_PER_MESSAGE);
        entries[..len].copy_from_slice(&helper.entries[..len]);
        Ok(ExecutionLogMessage {
            number_of_dropped_entries: Saturating(helper.number_of_dropped_entries),
            entries,
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
}

impl ExecutionLogDescriptor {
    /// Creates a descriptor from a slice of callback nodes.
    /// This should be called after all node have been added.
    pub fn new(nodes: &[CallbackNode]) -> ExecutionLogDescriptor {
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

        ExecutionLogDescriptor { index_to_callbacks }
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
        for node in pool.nodes.iter_mut() {
            let node_name = node.name().to_string();
            for subscriber in node.callback_mut().collect_subscribers_mut() {
                if subscriber.config().channel_name != EXECUTION_LOG_CHANNEL {
                    continue;
                }
                for log_pub in log_pubs.iter_mut() {
                    match log_pub.connect_to_subscriber(subscriber) {
                        Ok(()) => {}
                        Err(ConnectionTypeMismatch {}) => {
                            return Err(ExecutionLogConnectError {
                                channel_name: EXECUTION_LOG_CHANNEL.into(),
                                subscriber_node: node_name,
                            });
                        }
                    }
                }
            }
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
        assert_eq!(entry.next_free(), Some(0));
        assert!(entry.messages.iter().all(|m| !m.is_valid()));
    }

    #[test]
    fn sentinel_occupancy_walks_messages_then_next_entry() {
        let mut entry = ExecutionLogEntry {
            execution_time: FrameworkTime::from_nanoseconds(1),
            ..Default::default()
        };
        // Fill 3 of 24 message slots with valid headers; rest stay INVALID.
        for i in 0..3 {
            entry.messages[i] = LoggedMessage {
                ordinal: i as u16,
                direction: Direction::Received,
                header: MessageHeader::new(FrameworkTime::from_nanoseconds(10 + i as i64)),
            };
        }
        assert_eq!(entry.next_free(), Some(3));
        assert_eq!(entry.messages.iter().filter(|m| m.is_valid()).count(), 3);
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
