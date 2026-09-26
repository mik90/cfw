use std::num::Saturating;
use std::time::Duration;
use task::execution_log::{
    ENTRIES_PER_MESSAGE, ExecutionLogMessage, LoggedMessage, MESSAGES_PER_ENTRY,
};
#[cfg(feature = "iceoryx2")]
use task::execution_log::{ExecutionLogEntry, LoggedIox2Event};
use task::publisher::Publisher;
use task::scheduling::ReadyNodeSink;
use task::time::FrameworkTime;

pub(crate) struct WorkerLogger {
    publisher: Publisher<ExecutionLogMessage>,
    flush_period: Duration,
    last_flush: FrameworkTime,
    current_loan: Option<usize>,
    cur_node: u32,
    cur_time: FrameworkTime,
    cur_duration: Duration,
    next_entry: usize,
    next_msg: usize,
    dropped: Saturating<usize>,
    recv_scratch: Vec<LoggedMessage>,
    recv_scratch_len: usize,
}

pub(crate) struct WorkerLoggerInit {
    pub(crate) publisher: Publisher<ExecutionLogMessage>,
    pub(crate) flush_period: Duration,
    pub(crate) scratch_capacity: usize,
}

impl WorkerLogger {
    /// Allocate scratch storage before moving the publisher out of init, so a
    /// recoverable allocation panic leaves the publisher arena alive for cleanup.
    pub(crate) fn new(init: &mut Option<WorkerLoggerInit>, now: FrameworkTime) -> Option<Self> {
        let scratch_capacity = init.as_ref()?.scratch_capacity;
        let recv_scratch = Vec::with_capacity(scratch_capacity);
        let init = init.take().expect("checked above");
        Some(WorkerLogger {
            current_loan: None,
            publisher: init.publisher,
            flush_period: init.flush_period,
            last_flush: now,
            cur_node: 0,
            cur_time: FrameworkTime::INVALID,
            cur_duration: Duration::ZERO,
            next_entry: 0,
            next_msg: 0,
            dropped: Saturating(0),
            recv_scratch,
            recv_scratch_len: 0,
        })
    }

    pub(crate) fn has_data(&mut self) -> bool {
        if self.current_loan.is_none() {
            self.current_loan = self.publisher.loan_default().ok();
        }
        if self.current_loan.is_some() {
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    pub(crate) fn recv_scratch_clear(&mut self) {
        self.recv_scratch_len = 0;
    }

    pub(crate) fn recv_push(&mut self, msg: LoggedMessage) {
        if self.recv_scratch_len < self.recv_scratch.len() {
            self.recv_scratch[self.recv_scratch_len] = msg;
        } else {
            self.recv_scratch.push(msg);
        }
        self.recv_scratch_len += 1;
    }

    pub(crate) fn begin_execution(
        &mut self,
        node_index: u32,
        time: FrameworkTime,
        duration: Duration,
        sink: &mut dyn ReadyNodeSink,
    ) {
        // Finalize any partially-filled entry left by the previous execution
        // so each [`ExecutionLogEntry`] describes exactly one execution. The
        // replay side groups entries by `(callback_node_index, execution_time)`
        // and attributes every message in an entry to that execution; without
        // this roll, consecutive executions would share one entry and their
        // messages would be misattributed.
        self.roll_to_fresh_entry(sink);
        self.cur_node = node_index;
        self.cur_time = time;
        self.cur_duration = duration;
    }

    /// Move to a fresh, unused entry slot so the current execution's messages
    /// start a new entry. No-op when the current entry is already empty.
    fn roll_to_fresh_entry(&mut self, sink: &mut dyn ReadyNodeSink) {
        if self.next_msg == 0 {
            return;
        }
        self.next_entry += 1;
        self.next_msg = 0;
        if self.next_entry == ENTRIES_PER_MESSAGE && !self.flush_current(self.cur_time, sink) {
            // No loan available; `append` counts the drop. On success
            // `flush_current` already resets both cursors.
            self.next_entry = 0;
            self.next_msg = 0;
        }
    }

    /// Record a duration-only execution: a single entry carrying the node,
    /// execution time and duration with no messages. Consumers treat such
    /// entries as if there was no execution log at all.
    pub(crate) fn record_duration_only(
        &mut self,
        node_index: u32,
        time: FrameworkTime,
        duration: Duration,
        sink: &mut dyn ReadyNodeSink,
    ) {
        self.roll_to_fresh_entry(sink);
        let Some(loan) = self.current_loan else {
            self.dropped += 1;
            return;
        };
        let cur = self.publisher.loaned_payload_mut(loan);
        let entry = &mut cur.entries[self.next_entry];
        entry.callback_node_index = node_index;
        entry.execution_time = time;
        entry.execution_duration_ns = duration.as_nanos() as u64;
        entry.log_whole = false;
        self.next_entry += 1;
        self.next_msg = 0;
        if self.next_entry == ENTRIES_PER_MESSAGE {
            if !self.flush_current(time, sink) {
                self.dropped += 1;
            }
            self.next_entry = 0;
            self.next_msg = 0;
        }
    }

    /// Record an observed event for a callback's subscriber, independently of a callback run.
    #[cfg(feature = "iceoryx2")]
    pub(crate) fn record_iox2_event(
        &mut self,
        callback_node_index: u32,
        subscriber_ordinal: u16,
        event_id: usize,
        count: u64,
        observed_at: FrameworkTime,
        sink: &mut dyn ReadyNodeSink,
    ) {
        self.roll_to_fresh_entry(sink);
        if !self.has_data() {
            return;
        }
        let loan = self.current_loan.expect("has_data acquired a log loan");
        let current = self.publisher.loaned_payload_mut(loan);
        current.entries[self.next_entry] = ExecutionLogEntry {
            callback_node_index,
            execution_time: observed_at,
            iox2_event: Some(LoggedIox2Event {
                subscriber_ordinal,
                event_id,
                count,
                observed_at,
            }),
            ..Default::default()
        };
        self.next_entry += 1;
        if self.next_entry == ENTRIES_PER_MESSAGE {
            self.flush_current(observed_at, sink);
        }
    }

    pub(crate) fn append(&mut self, msg: LoggedMessage, sink: &mut dyn ReadyNodeSink) {
        let Some(loan) = self.current_loan else {
            self.dropped += 1;
            return;
        };

        if self.next_msg == MESSAGES_PER_ENTRY {
            self.next_entry += 1;
            self.next_msg = 0;
            if self.next_entry == ENTRIES_PER_MESSAGE {
                if !self.flush_current(self.cur_time, sink) {
                    self.dropped += 1;
                    self.next_entry = 0;
                    self.next_msg = 0;
                    return;
                }
                self.next_entry = 0;
            }
        }

        let cur = self.publisher.loaned_payload_mut(loan);
        let entry = &mut cur.entries[self.next_entry];
        if !entry.is_valid() {
            entry.callback_node_index = self.cur_node;
            entry.execution_time = self.cur_time;
            entry.execution_duration_ns = self.cur_duration.as_nanos() as u64;
            entry.log_whole = true;
        }
        entry.messages[self.next_msg] = msg;
        self.next_msg += 1;
    }

    pub(crate) fn drain_recv_into_current(&mut self, sink: &mut dyn ReadyNodeSink) {
        let len = self.recv_scratch_len;
        for i in 0..len {
            let msg = self.recv_scratch[i];
            self.append(msg, sink);
        }
        self.recv_scratch_len = 0;
    }

    pub(crate) fn maybe_flush_period(&mut self, now: FrameworkTime, sink: &mut dyn ReadyNodeSink) {
        let due = now
            .checked_duration_since(self.last_flush)
            .map(|d| d >= self.flush_period)
            .unwrap_or(false);
        if !due {
            return;
        }
        if self.next_entry > 0 || self.next_msg > 0 {
            self.flush_current(now, sink);
        }
    }

    fn flush_current(&mut self, at: FrameworkTime, sink: &mut dyn ReadyNodeSink) -> bool {
        let Some(loan) = self.current_loan else {
            return false;
        };
        if self.next_entry == 0 && self.next_msg == 0 {
            return true;
        }

        let cur = self.publisher.loaned_payload_mut(loan);
        cur.number_of_dropped_entries = self.dropped;
        self.dropped = Saturating(0);
        self.last_flush = at;

        self.publisher.mark_loan_sent(loan);
        task::generic_publisher::GenericPublisher::flush_loaned_values(
            &mut self.publisher,
            at,
            sink,
        );

        self.current_loan = self.publisher.loan_default().ok();
        self.next_entry = 0;
        self.next_msg = 0;
        self.current_loan.is_some()
    }

    pub(crate) fn flush_remaining(&mut self, at: FrameworkTime, sink: &mut dyn ReadyNodeSink) {
        if self.next_entry > 0 || self.next_msg > 0 {
            self.flush_current(at, sink);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use task::publisher::PublisherConfig;

    #[cfg(feature = "iceoryx2")]
    #[test]
    fn readiness_logger_batches_observations_with_their_recipient() {
        use task::generic_publisher::GenericPublisher as _;
        use task::subscriber::{Subscriber, SubscriberConfig};

        let mut publisher = Publisher::<ExecutionLogMessage>::new(PublisherConfig {
            capacity: 1,
            channel_name: task::execution_log::EXECUTION_LOG_CHANNEL.into(),
        });
        let subscriber = Subscriber::<ExecutionLogMessage>::new(SubscriberConfig {
            is_optional: true,
            capacity: 2,
            is_trigger: false,
            keep_across_runs: false,
            channel_name: task::execution_log::EXECUTION_LOG_CHANNEL.into(),
        });
        let mut subscriber = subscriber;
        publisher.connect_to_subscriber(&mut subscriber).unwrap();
        publisher.allocate_arena();
        let mut init = Some(WorkerLoggerInit {
            publisher,
            flush_period: Duration::from_secs(1),
            scratch_capacity: 0,
        });
        let mut logger = WorkerLogger::new(&mut init, FrameworkTime::from_nanoseconds(1)).unwrap();
        let observed_at = FrameworkTime::from_nanoseconds(15);
        logger.record_iox2_event(
            4,
            2,
            9,
            17,
            observed_at,
            &mut task::scheduling::NoopReadyNodeSink,
        );
        logger.record_iox2_event(
            5,
            0,
            9,
            17,
            observed_at,
            &mut task::scheduling::NoopReadyNodeSink,
        );
        logger.flush_remaining(observed_at, &mut task::scheduling::NoopReadyNodeSink);

        subscriber.drain_writer_to_reader();
        let buffer = subscriber.read_buffer();
        let batch = &buffer.front().unwrap().message;
        for (index, node) in [4, 5].into_iter().enumerate() {
            let entry = &batch.entries[index];
            assert_eq!(entry.callback_node_index, node);
            assert!(!entry.log_whole);
            let event = entry.iox2_event.unwrap();
            assert_eq!(event.subscriber_ordinal, if index == 0 { 2 } else { 0 });
            assert_eq!(event.event_id, 9);
            assert_eq!(event.count, 17);
            assert_eq!(event.observed_at, observed_at);
        }
        assert_eq!(batch.next_free_entry(), Some(2));
        drop(buffer);
        subscriber.cleanup_buffers();
    }

    #[test]
    fn initialization_panic_retains_logger_init() {
        let mut init = Some(WorkerLoggerInit {
            publisher: Publisher::new(PublisherConfig {
                capacity: 1,
                channel_name: "execution_log".into(),
            }),
            flush_period: Duration::from_secs(1),
            scratch_capacity: usize::MAX,
        });

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = WorkerLogger::new(&mut init, FrameworkTime::from_nanoseconds(0));
        }));

        assert!(result.is_err());
        assert!(
            init.is_some(),
            "publisher ownership must remain outside the unwind boundary"
        );
    }
}
