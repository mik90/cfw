use crate::pool_state::Scheduler;
use std::time::{Duration, Instant};

pub(crate) struct PeriodicNode {
    pub node: usize,
    pub next: Instant,
    pub period: Duration,
}

pub(crate) fn run(scheduler: &Scheduler, mut periodic: Vec<PeriodicNode>) {
    while !scheduler.is_stopped() {
        let now = Instant::now();
        for entry in &mut periodic {
            if entry.next <= now {
                scheduler.trigger(entry.node);
                // Coalesce missed periods rather than creating an unbounded backlog.
                entry.next = now
                    .checked_add(entry.period)
                    .expect("period exceeds clock range");
            }
        }
        let next = periodic
            .iter()
            .map(|entry| entry.next)
            .min()
            .expect("periodic list is nonempty");
        let _ = scheduler
            .stop_rx
            .recv_timeout(next.saturating_duration_since(Instant::now()));
    }
}
