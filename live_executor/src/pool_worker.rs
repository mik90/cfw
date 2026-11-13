use std::alloc::Allocator;
use std::sync::Mutex;
use task::execute_callback;
use task::executor::TimeSource;
use task_graph::{GraphMetadata, ScheduledCallback};

use crate::ThreadFailure;
use crate::pool_state::{PoolState, Scheduler};

pub(crate) fn work<T: TimeSource, A: Allocator + Clone + Send + Sync + 'static>(
    id: usize,
    pool: &PoolState,
    scheduler: &Scheduler<A>,
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
                let (executed, requeue) = {
                    let mut node = nodes[index].lock().unwrap();
                    let context = metadata.context(clock.now());
                    let before = node.callback.required_input_count();
                    let executed = execute_callback(node.callback.as_mut(), &context).map_err(|source| ThreadFailure::Callback {
                        worker: id, callback: node.name.clone(), source,
                    })?;
                    let after = node.callback.required_input_count();
                    let progressed = matches!(
                        (before, after),
                        (Some(before), Some(after)) if after < before
                    );
                    let requeue = executed
                        && progressed
                        && node.callback.has_triggering_inputs()
                        && node.callback.required_inputs_available();
                    if executed && node.schedule.is_timed() {
                        let next = node.schedule.next_after(clock.now()).map_err(|source| ThreadFailure::Timing { callback: node.name.clone(), source })?;
                        scheduler.set_deadline(index, next);
                    }
                    (executed, requeue)
                };
                scheduler.finish(index, executed);
                if requeue {
                    scheduler.trigger(index);
                }
            }
        }
    }
    Ok(())
}
