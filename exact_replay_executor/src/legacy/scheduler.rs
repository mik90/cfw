//! Execution-log-driven scheduling: iterates through parsed replay executions
//! in time order and yields them one at a time.

use crate::log_reader::ReplayExecution;

/// Simple sequential scheduler that yields executions in time order.
/// In the current deterministic first version, there is one replay worker
/// and no concurrency, so the scheduler is trivial.
pub(crate) struct ReplayScheduler {
    executions: Vec<ReplayExecution>,
    cursor: usize,
}

impl ReplayScheduler {
    pub fn new(executions: Vec<ReplayExecution>) -> Self {
        // Executions are already sorted by time from log_reader.
        ReplayScheduler {
            executions,
            cursor: 0,
        }
    }

    /// Advance to the next execution. Returns `None` when all executions
    /// have been consumed.
    pub fn advance(&mut self) -> Option<&ReplayExecution> {
        let idx = self.cursor;
        if idx >= self.executions.len() {
            return None;
        }
        self.cursor += 1;
        Some(&self.executions[idx])
    }

    /// Peek at the current execution without advancing.
    pub fn peek(&self) -> Option<&ReplayExecution> {
        self.executions.get(self.cursor)
    }

    /// Total number of executions.
    pub fn len(&self) -> usize {
        self.executions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.executions.is_empty()
    }

    /// How many executions have been consumed so far.
    pub fn consumed(&self) -> usize {
        self.cursor
    }

    /// Reset the scheduler to the beginning.
    pub fn reset(&mut self) {
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use task::time::FrameworkTime;

    fn make_execution(index: usize, time_ns: i64) -> ReplayExecution {
        ReplayExecution {
            callback_node_index: index,
            execution_time: FrameworkTime::from_nanoseconds(time_ns),
            execution_duration_ns: 0,
            received: HashMap::new(),
            published: HashMap::new(),
        }
    }

    #[test]
    fn scheduler_iterates_in_order() {
        let execs = vec![
            make_execution(0, 100),
            make_execution(1, 200),
            make_execution(0, 300),
        ];
        let mut sched = ReplayScheduler::new(execs);
        assert_eq!(sched.len(), 3);

        let e1 = sched.advance().unwrap();
        assert_eq!(e1.callback_node_index, 0);
        assert_eq!(e1.execution_time.to_nanoseconds(), 100);

        let e2 = sched.advance().unwrap();
        assert_eq!(e2.callback_node_index, 1);

        let e3 = sched.advance().unwrap();
        assert_eq!(e3.callback_node_index, 0);

        assert!(sched.advance().is_none());
        assert_eq!(sched.consumed(), 3);
    }

    #[test]
    fn scheduler_peek() {
        let execs = vec![make_execution(0, 100)];
        let mut sched = ReplayScheduler::new(execs);
        assert_eq!(sched.peek().unwrap().callback_node_index, 0);
        assert_eq!(sched.consumed(), 0);
        let _ = sched.advance();
        assert!(sched.peek().is_none());
    }

    #[test]
    fn scheduler_reset() {
        let execs = vec![make_execution(0, 100), make_execution(1, 200)];
        let mut sched = ReplayScheduler::new(execs);
        sched.advance();
        sched.advance();
        assert_eq!(sched.consumed(), 2);
        sched.reset();
        assert_eq!(sched.consumed(), 0);
        assert!(sched.advance().is_some());
    }
}
