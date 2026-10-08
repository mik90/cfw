use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Arc, Mutex};
use std::thread;
use task::executor::{TimeSource, WallClock};
use task::{BuiltGraph, GraphMetadata, ScheduledCallback, execute_callback};

use crate::pool_state::{PoolState, Scheduler};
use crate::{LiveExecutorError, LiveExecutorStartError, StopSignal, ThreadFailure};

/// Native live execution borrowing externally owned graph storage. `run_with`
/// owns the thread scope and joins all workers before returning or unwinding.
pub struct LiveExecutor<'storage, T: TimeSource = WallClock> {
    nodes: Vec<ScheduledCallback<'storage>>,
    metadata: GraphMetadata,
    scheduler: Arc<Scheduler>,
    clock: T,
}

impl<'storage> LiveExecutor<'storage, WallClock> {
    pub fn new(
        thread_count: usize,
        graph: BuiltGraph<'storage>,
    ) -> Result<Self, LiveExecutorStartError> {
        Self::new_multi_pool_with_time(vec![thread_count], graph, WallClock)
    }

    pub fn new_multi_pool(
        workers: Vec<usize>,
        graph: BuiltGraph<'storage>,
    ) -> Result<Self, LiveExecutorStartError> {
        Self::new_multi_pool_with_time(workers, graph, WallClock)
    }
}

impl<'storage, T: TimeSource> LiveExecutor<'storage, T> {
    pub fn new_multi_pool_with_time(
        workers: Vec<usize>,
        graph: BuiltGraph<'storage>,
        clock: T,
    ) -> Result<Self, LiveExecutorStartError> {
        if workers.is_empty() || workers.contains(&0) {
            return Err(LiveExecutorStartError {
                reason: "each executor pool must have at least one worker".into(),
            });
        }
        let (nodes, metadata) = graph.into_parts();
        if let Some(node) = nodes
            .iter()
            .find(|node| node.schedule.pool >= workers.len())
        {
            return Err(LiveExecutorStartError {
                reason: format!(
                    "callback '{}' refers to missing pool {}",
                    node.name, node.schedule.pool
                ),
            });
        }
        let schedules: Vec<_> = nodes.iter().map(|node| node.schedule.clone()).collect();
        let scheduler = Scheduler::new(&schedules, &workers);
        Ok(Self {
            nodes,
            metadata,
            scheduler,
            clock,
        })
    }

    pub fn stop_signal(&self) -> StopSignal {
        StopSignal(self.scheduler.clone())
    }

    /// Wait for an external stop request or a worker failure.
    pub fn run(self) -> Result<(), LiveExecutorError> {
        self.run_with(|stop| stop.wait())
    }

    /// Run the controller on the calling thread while scoped workers execute.
    /// Returning from the controller requests shutdown. Controllers blocking on
    /// external signals should also observe `stop.receiver()` for worker failures.
    /// A controller panic is resumed only after all threads have joined.
    pub fn run_with<R>(
        self,
        controller: impl FnOnce(&StopSignal) -> R,
    ) -> Result<R, LiveExecutorError> {
        self.run_checked(controller, |_| Ok(()))
    }

    fn run_checked<R>(
        self,
        controller: impl FnOnce(&StopSignal) -> R,
        mut before_spawn: impl FnMut(usize) -> std::io::Result<()>,
    ) -> Result<R, LiveExecutorError> {
        let Self {
            mut nodes,
            metadata,
            scheduler,
            clock,
        } = self;
        let stop = StopSignal(scheduler.clone());
        let _stop_on_exit = StopOnExit(stop.clone());
        let started = clock.now();
        let mut periodic = Vec::new();
        #[cfg(feature = "iceoryx2")]
        let mut registrations = Vec::new();
        for (index, node) in nodes.iter_mut().enumerate() {
            if node.schedule.is_timed() {
                let next = if node.schedule.run_on_start {
                    None
                } else {
                    node.schedule.next_after(started).map_err(|error| {
                        LiveExecutorError::Start(LiveExecutorStartError {
                            reason: format!("invalid timing for '{}': {error:?}", node.name),
                        })
                    })?
                };
                scheduler.set_deadline(index, next);
                periodic.push(index);
            }
            node.callback.set_waker(scheduler.waker(index));
            #[cfg(feature = "iceoryx2")]
            registrations.extend(node.callback.take_iox2_events());
        }
        #[cfg(feature = "iceoryx2")]
        let shutdown = if registrations.is_empty() {
            None
        } else {
            let shutdown = task::iox2::Iox2Shutdown::new().map_err(|error| {
                LiveExecutorError::Start(LiveExecutorStartError {
                    reason: format!("{error:?}"),
                })
            })?;
            scheduler.set_external_stop(shutdown.wake.clone());
            Some(shutdown)
        };
        let initial: Vec<_> = nodes
            .iter()
            .enumerate()
            .filter_map(|(i, node)| {
                (node.schedule.run_on_start || node.callback.has_pending_inputs()).then_some(i)
            })
            .collect();
        let nodes: Vec<_> = nodes.into_iter().map(Mutex::new).collect();

        thread::scope(|scope| {
            // This guard must unwind inside the scope, before scope's implicit
            // joins, including a panic during thread setup or initial scheduling.
            let _scope_stop = StopOnExit(stop.clone());
            let mut handles = Vec::new();
            let mut spawn_index = 0;
            let mut startup_error = None;
            #[cfg(feature = "iceoryx2")]
            if let Some(shutdown) = shutdown {
                let (ready, attached) = crossbeam::channel::bounded(1);
                let stop = stop.clone();
                let scheduler = &scheduler;
                let spawn = before_spawn(spawn_index).and_then(|()| {
                    thread::Builder::new()
                        .name("cfw_iox2_readiness".into())
                        .spawn_scoped(scope, move || {
                            let _stop_on_exit = StopOnExit(stop);
                            crate::readiness::run(registrations, shutdown, scheduler, ready)
                        })
                });
                match spawn {
                    Ok(handle) => {
                        handles.push(("cfw_iox2_readiness".into(), handle));
                        match attached.recv() {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => startup_error = Some(std::io::Error::other(error)),
                            Err(error) => startup_error = Some(std::io::Error::other(error)),
                        }
                    }
                    Err(error) => startup_error = Some(error),
                }
                spawn_index += 1;
            }
            if startup_error.is_none() {
                'pools: for (pool_index, pool) in scheduler.pools.iter().enumerate() {
                    for worker_index in 0..pool.thread_count {
                        let name = format!("cfw_pool_{pool_index}_t_{worker_index}");
                        let id = spawn_index;
                        let stop = stop.clone();
                        let nodes = &nodes;
                        let metadata = &metadata;
                        let clock = &clock;
                        let scheduler = &scheduler;
                        let spawn = before_spawn(spawn_index).and_then(|()| {
                            thread::Builder::new().name(name.clone()).spawn_scoped(
                                scope,
                                move || {
                                    let _stop_on_exit = StopOnExit(stop);
                                    worker(id, pool, scheduler, nodes, metadata, clock)
                                },
                            )
                        });
                        match spawn {
                            Ok(handle) => handles.push((name, handle)),
                            Err(error) => {
                                startup_error = Some(error);
                                break 'pools;
                            }
                        }
                        spawn_index += 1;
                    }
                }
            }
            if startup_error.is_none() && !periodic.is_empty() {
                let scheduler = &scheduler;
                let stop = stop.clone();
                let clock = &clock;
                let spawn = before_spawn(spawn_index).and_then(|()| {
                    thread::Builder::new()
                        .name("cfw_periodic".into())
                        .spawn_scoped(scope, move || {
                            let _stop_on_exit = StopOnExit(stop);
                            crate::periodic::run(scheduler, periodic, clock);
                            Ok(())
                        })
                });
                match spawn {
                    Ok(handle) => handles.push(("cfw_periodic".into(), handle)),
                    Err(error) => startup_error = Some(error),
                }
            }
            let result = if startup_error.is_none() {
                for index in initial {
                    scheduler.trigger(index);
                }
                Some(catch_unwind(AssertUnwindSafe(|| controller(&stop))))
            } else {
                None
            };
            stop.request_stop();
            let mut failures = Vec::new();
            for (name, handle) in handles {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(failure)) => failures.push(failure),
                    Err(_) => failures.push(ThreadFailure::Panic { thread: name }),
                }
            }
            if let Some(error) = startup_error {
                return Err(LiveExecutorError::Start(LiveExecutorStartError {
                    reason: error.to_string(),
                }));
            }
            match result.expect("controller runs after successful startup") {
                Err(payload) => resume_unwind(payload),
                Ok(value) if failures.is_empty() => Ok(value),
                Ok(_) => Err(LiveExecutorError::Threads(failures)),
            }
        })
    }
}

struct StopOnExit(StopSignal);
impl Drop for StopOnExit {
    fn drop(&mut self) {
        self.0.request_stop();
    }
}

fn worker<T: TimeSource>(
    id: usize,
    pool: &PoolState,
    scheduler: &Scheduler,
    nodes: &[Mutex<ScheduledCallback<'_>>],
    metadata: &GraphMetadata,
    clock: &T,
) -> Result<(), ThreadFailure> {
    while !scheduler.is_stopped() {
        crossbeam::select! {
            recv(scheduler.stop_rx) -> _ => return Ok(()),
            recv(pool.work_rx) -> work => {
                let index = work.expect("scheduler owns work sender");
                if scheduler.is_stopped() { return Ok(()); }
                scheduler.claim(index);
                let executed = {
                    let mut node = nodes[index].lock().unwrap();
                    let context = metadata.context(clock.now());
                    let executed = execute_callback(node.callback.as_mut(), &context).map_err(|source| ThreadFailure::Callback {
                        worker: id, callback: node.name.clone(), source,
                    })?;
                    if executed && node.schedule.is_timed() {
                        let next = node.schedule.next_after(clock.now()).map_err(|source| ThreadFailure::Timing { callback: node.name.clone(), source })?;
                        scheduler.set_deadline(index, next);
                    }
                    executed
                };
                scheduler.finish(index, executed);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use task::{CallbackSchedule, GraphBuilder};

    struct Clock;
    impl TimeSource for Clock {
        fn now(&self) -> task::time::FrameworkTime {
            task::time::FrameworkTime::from_nanoseconds(0)
        }
    }
    struct Probe(Arc<AtomicUsize>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    #[cfg(feature = "iceoryx2")]
    #[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
    fn worker_spawn_failure_joins_already_attached_readiness_thread() {
        use task::iox2::{Iox2ChannelPlan, Iox2EventBindings, Iox2EventSubscriber, Iox2Runtime};
        struct EventCallback(Iox2EventSubscriber);
        impl task::Callback for EventCallback {
            fn set_waker(&mut self, wake: task::wake::WakeHandle) {
                self.0.set_waker(wake);
            }
            fn take_iox2_events(&mut self) -> Vec<task::iox2::Iox2EventRegistration> {
                self.0.take_registration().into_iter().collect()
            }
            fn run(&mut self, _: &task::Context) -> Result<(), task::LoanError> {
                Ok(())
            }
        }
        let runtime = Iox2Runtime::new().unwrap();
        let mut plan = Iox2ChannelPlan::<u64>::new(
            format!("cfw_startup_ipc_{}", std::process::id()),
            &runtime,
        );
        let event = plan.events(1);
        let storage = task::GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build().unwrap();
        let mut builder = GraphBuilder::new();
        builder.add_callback("events", || Ok(EventCallback(bindings.take_event(&event)?)));
        let executor =
            LiveExecutor::new_multi_pool_with_time(vec![1], builder.build().unwrap(), Clock)
                .unwrap();
        let stop = executor.stop_signal();
        let result = executor.run_checked(
            |_| panic!("startup should have failed"),
            |index| {
                if index == 1 {
                    Err(std::io::Error::other(
                        "worker spawn failed after readiness attached",
                    ))
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(LiveExecutorError::Start(_))));
        assert!(stop.is_stopped());
    }

    #[test]
    fn startup_failure_or_panic_joins_partial_worker_set() {
        for (periodic, panic) in [(false, false), (true, false), (false, true)] {
            let drops = Arc::new(AtomicUsize::new(0));
            let probe = Probe(drops.clone());
            let mut builder = GraphBuilder::new();
            let schedule = if periodic {
                CallbackSchedule::periodic(Duration::from_secs(3600))
            } else {
                CallbackSchedule::default()
            };
            builder.add_scheduled_callback("probe", schedule, || {
                Ok(move |_| {
                    let _ = &probe;
                    Ok(())
                })
            });
            let executor = LiveExecutor::new_multi_pool_with_time(
                vec![if periodic { 1 } else { 2 }],
                builder.build().unwrap(),
                Clock,
            )
            .unwrap();
            let stop = executor.stop_signal();
            let result = catch_unwind(AssertUnwindSafe(|| {
                executor.run_checked(
                    |_| panic!("controller must not run after startup failure"),
                    |index| {
                        if index == 1 {
                            assert!(!panic, "injected startup panic");
                            return Err(std::io::Error::other("injected spawn failure"));
                        }
                        Ok(())
                    },
                )
            }));
            if panic {
                assert!(result.is_err());
            } else {
                assert!(matches!(result.unwrap(), Err(LiveExecutorError::Start(_))));
            }
            assert!(stop.is_stopped());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }
}
