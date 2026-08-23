use crate::pool_state::LiveReadyNodeSink;
use std::collections::VecDeque;
use std::sync::Arc;
use task::callback_storage::SharedCallbackNode;
use task::executor::TimeSource;
use task::scheduling::{CallbackNodeId, ReadyNodeSink};
use task::time::FrameworkTime;

use crate::pool_state::{SharedThreadPoolState, TimeTriggeredNode};

pub(crate) fn periodic_trigger_thread<T: TimeSource>(
    shared_state: &SharedThreadPoolState,
    nodes: &[Arc<SharedCallbackNode>],
    exec_times: &mut VecDeque<TimeTriggeredNode>,
    time_source: &T,
) {
    let now = time_source.now();

    let maybe_earliest = exec_times
        .iter()
        .min_by_key(|node| node.requested_exec_time)
        .copied();

    let time_triggered_node = match maybe_earliest {
        Some(t) => t,
        None => {
            let guard = shared_state.periodic_mutex.lock().unwrap();
            drop(
                shared_state
                    .periodic_cond_var
                    .wait_while(guard, |_| {
                        shared_state
                            .should_run
                            .load(std::sync::atomic::Ordering::Acquire)
                    })
                    .unwrap(),
            );
            return;
        }
    };

    if let Some(duration) = time_triggered_node
        .requested_exec_time
        .checked_duration_since(now)
    {
        let guard = shared_state.periodic_mutex.lock().unwrap();
        let _ = shared_state
            .periodic_cond_var
            .wait_timeout_while(guard, duration, |_| {
                shared_state
                    .should_run
                    .load(std::sync::atomic::Ordering::Acquire)
            })
            .unwrap();
    }

    let now = time_source.now();

    if now >= time_triggered_node.requested_exec_time {
        // If a worker is currently running the node, wait briefly and
        // re-evaluate on the next loop iteration — the node is already
        // executing, so there is nothing to trigger, and its (still-past-due)
        // entry will be re-fired once free.
        if nodes[time_triggered_node.index].is_running() {
            let guard = shared_state.periodic_mutex.lock().unwrap();
            let _ = shared_state
                .periodic_cond_var
                .wait_timeout(guard, std::time::Duration::from_micros(100))
                .unwrap();
            return;
        }

        // Snapshot the next requested execution time from the node's atomic —
        // reading node internals cross-thread is unnecessary and unsafe. The
        // snapshot is refreshed by the worker after each run via `execute()`,
        // so it may be computed relative to the last execution's `now`; that
        // slight staleness is acceptable for advisory scheduling.
        let next_exec_time = nodes[time_triggered_node.index]
            .next_exec_time()
            .unwrap_or(FrameworkTime::MAX);

        for node in exec_times.iter_mut() {
            if node.index == time_triggered_node.index {
                node.requested_exec_time = next_exec_time;
            }
        }

        let mut sink = LiveReadyNodeSink {
            nodes,
            router: &shared_state.work_router,
        };
        sink.schedule(CallbackNodeId(time_triggered_node.index));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, OnceLock,
            atomic::{AtomicUsize, Ordering},
        },
        thread::sleep,
        time,
    };

    use task::{
        callback::{Callback, CallbackNode, PortMut},
        context::Context,
        executor::{Executor, ExecutorStopSignal},
        generic_publisher::GenericPublisher,
        generic_subscriber::GenericSubscriber,
    };

    use crate::LiveExecutor;

    struct PeriodicCounter {
        run_count: Arc<AtomicUsize>,
        target_runs: usize,
        stop_signal: Arc<OnceLock<Arc<dyn ExecutorStopSignal>>>,
    }

    impl Callback for PeriodicCounter {
        fn run(&mut self, _ctx: &Context) -> task::callback::Run {
            let run_number = self.run_count.fetch_add(1, Ordering::SeqCst) + 1;
            if run_number >= self.target_runs
                && let Some(signal) = self.stop_signal.get()
            {
                signal.request_stop();
            }
            task::callback::Run::new(1)
        }

        fn for_each_subscriber<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericSubscriber)) {}
        fn for_each_publisher<'a>(&'a self, _f: &mut dyn FnMut(&'a dyn GenericPublisher)) {}
        fn for_each_subscriber_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericSubscriber),
        ) {
        }
        fn for_each_publisher_mut<'a>(
            &'a mut self,
            _f: &mut dyn FnMut(&'a mut dyn GenericPublisher),
        ) {
        }
        fn for_each_port_mut<'a>(&'a mut self, _f: &mut dyn FnMut(PortMut<'a>)) {}
    }

    #[test]
    fn test_sustained_periodic_trigger() {
        #[cfg(not(miri))]
        const TARGET_RUNS: usize = 50;
        #[cfg(miri)]
        const TARGET_RUNS: usize = 5;

        #[cfg(not(miri))]
        const DEADLINE_SECS: u64 = 10;
        #[cfg(miri)]
        const DEADLINE_SECS: u64 = 120;

        let run_count = Arc::new(AtomicUsize::new(0));
        let stop_signal_cell = Arc::new(OnceLock::new());

        let callback: Box<dyn Callback> = Box::new(PeriodicCounter {
            run_count: run_count.clone(),
            target_runs: TARGET_RUNS,
            stop_signal: stop_signal_cell.clone(),
        });
        let mut connected = CallbackNode::new_named(callback, "PeriodicCounter".into());
        connected.set_execution_time_callback(Box::new(|now| {
            Some(now + time::Duration::from_millis(2))
        }));
        connected.set_execution_duration_callback(Box::new(|| time::Duration::ZERO));

        let mut exec = LiveExecutor::new(1, vec![connected]);

        stop_signal_cell.set(exec.stop_signal()).ok();
        exec.start_threads();

        let deadline = time::Instant::now() + time::Duration::from_secs(DEADLINE_SECS);
        while exec.is_running() && time::Instant::now() < deadline {
            sleep(time::Duration::from_millis(10));
        }
        assert!(
            !exec.is_running(),
            "Executor did not reach {TARGET_RUNS} periodic runs within {DEADLINE_SECS} seconds (stuck at {})",
            run_count.load(Ordering::SeqCst)
        );

        let stop_result = exec.stop_threads();
        assert!(stop_result.is_ok());

        assert!(run_count.load(Ordering::SeqCst) >= TARGET_RUNS);
    }
}
