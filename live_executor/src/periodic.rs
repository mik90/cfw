use crate::pool_state::Scheduler;
use std::time::Duration;
use task::executor::TimeSource;

pub(crate) fn run<T: TimeSource>(scheduler: &Scheduler, timed: Vec<usize>, clock: &T) {
    while !scheduler.is_stopped() {
        while scheduler.timer_rx.try_recv().is_ok() {}
        let now = clock.now();
        for &node in &timed {
            scheduler.dispatch_due(node, now);
        }
        let next = scheduler
            .timers_enabled()
            .then(|| {
                timed
                    .iter()
                    .filter_map(|&node| scheduler.deadline(node))
                    .min()
            })
            .flatten();
        // Re-check injected/scaled clocks even without a callback completion.
        let timeout = next.map_or(Duration::from_millis(100), |next| {
            let nanos = (i128::from(next.to_nanoseconds()) - i128::from(now.to_nanoseconds()))
                .max(0) as u64;
            Duration::from_nanos(nanos).min(Duration::from_millis(100))
        });
        crossbeam::select! {
            recv(scheduler.stop_rx) -> _ => break,
            recv(scheduler.timer_rx) -> _ => {},
            default(timeout) => {},
        }
    }
}
