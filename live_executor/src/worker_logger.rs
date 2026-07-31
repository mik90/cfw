use std::num::Saturating;
use std::time::Duration;
use task::callback::CallbackNode;
use task::execution_log::{
    ENTRIES_PER_MESSAGE, ExecutionLogMessage, LoggedMessage, MESSAGES_PER_ENTRY,
};
use task::generic_publisher::GenericPublisher;
use task::publisher::Publisher;
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
    pub(crate) fn new(init: WorkerLoggerInit, now: FrameworkTime) -> Self {
        let mut publisher = init.publisher;
        let current_loan = publisher.loan_default().ok();
        WorkerLogger {
            current_loan,
            publisher,
            flush_period: init.flush_period,
            last_flush: now,
            cur_node: 0,
            cur_time: FrameworkTime::INVALID,
            cur_duration: Duration::ZERO,
            next_entry: 0,
            next_msg: 0,
            dropped: Saturating(0),
            recv_scratch: Vec::with_capacity(init.scratch_capacity),
            recv_scratch_len: 0,
        }
    }

    pub(crate) fn captures_for(&mut self, node: &CallbackNode) -> bool {
        if !node.log_executions() {
            return false;
        }
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
    ) {
        // Finalize any partially-filled entry left by the previous execution
        // so each [`ExecutionLogEntry`] describes exactly one execution. The
        // replay side groups entries by `(callback_node_index, execution_time)`
        // and attributes every message in an entry to that execution; without
        // this roll, consecutive executions would share one entry and their
        // messages would be misattributed.
        self.roll_to_fresh_entry();
        self.cur_node = node_index;
        self.cur_time = time;
        self.cur_duration = duration;
    }

    /// Move to a fresh, unused entry slot so the current execution's messages
    /// start a new entry. No-op when the current entry is already empty.
    fn roll_to_fresh_entry(&mut self) {
        if self.next_msg == 0 {
            return;
        }
        self.next_entry += 1;
        self.next_msg = 0;
        if self.next_entry == ENTRIES_PER_MESSAGE && !self.flush_current(self.cur_time) {
            // No loan available; `append` counts the drop. On success
            // `flush_current` already resets both cursors.
            self.next_entry = 0;
            self.next_msg = 0;
        }
    }

    pub(crate) fn append(&mut self, msg: LoggedMessage) {
        let Some(loan) = self.current_loan else {
            self.dropped += 1;
            return;
        };

        if self.next_msg == MESSAGES_PER_ENTRY {
            self.next_entry += 1;
            self.next_msg = 0;
            if self.next_entry == ENTRIES_PER_MESSAGE {
                if !self.flush_current(self.cur_time) {
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
        }
        entry.messages[self.next_msg] = msg;
        self.next_msg += 1;
    }

    pub(crate) fn drain_recv_into_current(&mut self) {
        let len = self.recv_scratch_len;
        for i in 0..len {
            let msg = self.recv_scratch[i];
            self.append(msg);
        }
        self.recv_scratch_len = 0;
    }

    pub(crate) fn maybe_flush_period(&mut self, now: FrameworkTime) {
        let due = now
            .checked_duration_since(self.last_flush)
            .map(|d| d >= self.flush_period)
            .unwrap_or(false);
        if !due {
            return;
        }
        if self.next_entry > 0 || self.next_msg > 0 {
            self.flush_current(now);
        }
    }

    fn flush_current(&mut self, at: FrameworkTime) -> bool {
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
        self.publisher.flush_loaned_values(at);

        self.current_loan = self.publisher.loan_default().ok();
        self.next_entry = 0;
        self.next_msg = 0;
        self.current_loan.is_some()
    }

    pub(crate) fn flush_remaining(&mut self, at: FrameworkTime) {
        if self.next_entry > 0 || self.next_msg > 0 {
            self.flush_current(at);
        }
    }
}
