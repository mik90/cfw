use task::executor::ThreadPoolConfig;

use crate::callback_executor::{
    CallbackExecutionRequest, CallbackExecutionResponse, callback_executor_thread,
};
use crate::{
    ConnectedCallback, FrameworkTime, PoolIndex, SimulationConfig, TaskIndex, TimeTriggeredTask,
    VirtualPool,
};
use std::collections::VecDeque;
use std::num::Saturating;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct SimulationState {
    /// Storage of all tasks. A task's index into this vec is used to index into other Vecs.
    /// Each task is wrapped in a Mutex so the parallel callback executors can lock individual
    /// tasks without unsafe disjoint-index tricks.
    tasks: Vec<Arc<Mutex<ConnectedCallback>>>,

    /// Threads that execute callbacks in parallel.
    callback_executor_threads: Vec<thread::JoinHandle<()>>,

    /// Tells callback executors to do work
    callback_exec_request_senders: Vec<Sender<CallbackExecutionRequest>>,

    /// Mono channel with all results of execution
    callback_exec_response_receiver: Receiver<CallbackExecutionResponse>,

    /// Maps each global task index to its pool index
    task_to_pool: Vec<PoolIndex>,

    /// Virtual pools — no real threads, but models concurrency boundaries
    virtual_pools: Vec<VirtualPool>,

    /// TODO should this be a sorted queue?
    periodic_tasks: VecDeque<TimeTriggeredTask>,

    /// Per-task sim-time when each task's last execution finishes. Initialized to start_time.
    task_busy_until: Vec<FrameworkTime>,

    /// Sim-time when each task first became ready but hadn't yet been allocated a thread.
    /// None if the task is not currently waiting. Used to prioritize longest-waiting tasks.
    task_ready_since: Vec<Option<FrameworkTime>>,

    /// Current simulation time
    time: FrameworkTime,

    /// Number of times the state has been stepped
    step_count: Saturating<usize>,
}

impl Drop for SimulationState {
    fn drop(&mut self) {
        // Make sure callback threads exit even if stop() was never called.
        let _ = self.shutdown_callback_threads();
        self.cleanup();
    }
}

impl SimulationState {
    /// Create a single virtual pool with `num_virtual_threads` for all tasks,
    /// starting at simulation time zero
    pub fn new(num_virtual_threads: usize, tasks: Vec<ConnectedCallback>) -> Self {
        Self::new_with(SimulationConfig {
            start_time: FrameworkTime::from_nanoseconds(0),
            pools: vec![ThreadPoolConfig::new(num_virtual_threads, tasks)],
            callback_executor_thread_count: 1,
        })
    }

    /// Create an new state from a [`SimulationConfig`], supporting multiple virtual
    /// pools and a configurable start time.
    pub fn new_with(config: SimulationConfig) -> SimulationState {
        let mut all_tasks: Vec<Arc<Mutex<ConnectedCallback>>> = vec![];
        let mut task_to_pool: Vec<usize> = Vec::new();
        let mut virtual_pools: Vec<VirtualPool> = Vec::new();

        for (pool_idx, pool) in config.pools.into_iter().enumerate() {
            virtual_pools.push(VirtualPool {
                virtual_thread_count: pool.thread_count,
                num_threads_occupied: 0,
            });
            for task in pool.tasks {
                task_to_pool.push(pool_idx);
                all_tasks.push(Arc::new(Mutex::new(task)));
            }
        }

        let num_tasks = all_tasks.len();

        // One response channel shared by all callback executor threads
        let (exec_response_sender, exec_response_recv): (
            Sender<CallbackExecutionResponse>,
            Receiver<CallbackExecutionResponse>,
        ) = mpsc::channel();

        let mut state = SimulationState {
            tasks: all_tasks,
            callback_executor_threads: Vec::with_capacity(config.callback_executor_thread_count),
            callback_exec_request_senders: Vec::with_capacity(
                config.callback_executor_thread_count,
            ),
            callback_exec_response_receiver: exec_response_recv,
            virtual_pools,
            task_to_pool,
            periodic_tasks: VecDeque::new(),
            task_busy_until: vec![config.start_time; num_tasks],
            task_ready_since: vec![None; num_tasks],
            time: config.start_time,
            step_count: Saturating(0),
        };
        for _ in 0..config.callback_executor_thread_count {
            // Each thread has its own request receiver; the state owns the matching sender
            let (request_sender, request_recv): (
                Sender<CallbackExecutionRequest>,
                Receiver<CallbackExecutionRequest>,
            ) = mpsc::channel();
            state.callback_exec_request_senders.push(request_sender);

            let cloned_tasks = state.tasks.clone();

            let response_sender_clone = exec_response_sender.clone();
            state.callback_executor_threads.push(thread::spawn(move || {
                callback_executor_thread(request_recv, response_sender_clone, cloned_tasks);
            }));
        }

        state
    }

    pub fn start(&mut self) {
        // Set up periodic execution
        for (index, callback) in self.tasks.iter().enumerate() {
            if callback
                .lock()
                .unwrap()
                .get_next_requested_execution_time(self.time)
                .is_some()
            {
                self.periodic_tasks.push_back(TimeTriggeredTask {
                    index,
                    // Periodic tasks will run on startup, and then their requested times will be honored
                    requested_exec_time: self.time,
                });
            }
        }
    }

    /// Finds tasks that should run this step, allocates a thread from their pool to each,
    /// and drains their subscribers (write → read) so data is available when they run.
    ///
    /// A task is a candidate if it has new trigger data in its write buffer and isn't busy,
    /// or it is a periodic task that is due and isn't busy. Among candidates, only those
    /// whose pool has a free thread are returned. Drain is deferred until after thread
    /// allocation so that tasks which can't run don't consume their trigger data.
    fn allocate_tasks_to_threads(&mut self) -> Vec<TaskIndex> {
        let mut candidates: Vec<TaskIndex> = vec![];

        for index in 0..self.tasks.len() {
            if self.tasks[index]
                .lock()
                .unwrap()
                .subscribers_request_execution()
                && self.time >= self.task_busy_until[index]
            {
                candidates.push(index);
            }
        }
        for periodic in &self.periodic_tasks {
            if periodic.requested_exec_time <= self.time
                && self.time >= self.task_busy_until[periodic.index]
                && !candidates.contains(&periodic.index)
            {
                candidates.push(periodic.index);
            }
        }

        // Record when each task first became ready, then sort by wait time (oldest first)
        // with task index as a tiebreaker to preserve determinism.
        for &index in &candidates {
            self.task_ready_since[index].get_or_insert(self.time);
        }
        candidates.sort_by_key(|&index| (self.task_ready_since[index], index));

        let mut runnable: Vec<TaskIndex> = vec![];
        for index in candidates {
            let pool_index = self.task_to_pool[index];
            let pool = &mut self.virtual_pools[pool_index];
            if pool.num_threads_occupied < pool.virtual_thread_count {
                pool.num_threads_occupied += 1;
                self.task_ready_since[index] = None;
                runnable.push(index);
            }
        }

        runnable
    }

    pub fn step(&mut self) -> Vec<TaskIndex> {
        let runnable_tasks = self.allocate_tasks_to_threads();
        // Only drain subscribers for tasks that actually got a thread, so that tasks
        // blocked by pool pressure keep their trigger data for the next step.
        for &index in &runnable_tasks {
            self.tasks[index].lock().unwrap().drain_subscribers();
        }

        let time = self.time;

        let mut sender_cycle_iter = self.callback_exec_request_senders.iter().cycle();
        for index in &runnable_tasks {
            // Round-robin work across callback executor threads.
            sender_cycle_iter
                .next()
                .expect("No senders are in callback executor")
                .send(CallbackExecutionRequest {
                    index: *index,
                    current_time: time,
                    should_run: true,
                })
                .expect("Could not send execution request to callback thread");
        }

        let mut execution_responses: Vec<CallbackExecutionResponse> = vec![];
        for _ in &runnable_tasks {
            let response = self
                .callback_exec_response_receiver
                .recv()
                .expect("Could not receive response from callback thread");
            execution_responses.push(response);
        }

        // If there's more responses left, that's unexpected
        if let Ok(r) = self.callback_exec_response_receiver.try_recv() {
            panic!("Received unexpected response: {:?}", r);
        }

        for response in execution_responses {
            self.task_busy_until[response.index] = time + response.execution_duration;
        }

        for &index in &runnable_tasks {
            self.tasks[index].lock().unwrap().flush_publishers(time);
        }

        // Update periodic task next-run times from their no-longer-busy instant
        for periodic in &mut self.periodic_tasks {
            if runnable_tasks.contains(&periodic.index) {
                let no_longer_busy = self.task_busy_until[periodic.index];
                if let Some(next_time) = self.tasks[periodic.index]
                    .lock()
                    .unwrap()
                    .get_next_requested_execution_time(no_longer_busy)
                {
                    periodic.requested_exec_time = next_time;
                }
            }
        }

        let old_sim_time = self.time;

        // Advance sim time to earliest next event
        let next_busy = runnable_tasks
            .iter()
            .map(|&i| self.task_busy_until[i])
            .min();
        let next_periodic = self
            .periodic_tasks
            .iter()
            .map(|p| p.requested_exec_time)
            .filter(|&t| t > self.time)
            .min();
        if let Some(t) = [next_busy, next_periodic].into_iter().flatten().min()
            && t > self.time
        {
            self.time = t;
        }

        // See if any tasks are no longer busy, and if they aren't, free up a thread from their pool
        for (index, t) in self.task_busy_until.iter().enumerate() {
            let busy_until_time = *t;
            if busy_until_time > old_sim_time && busy_until_time <= self.time {
                // task is no longer busy as of the new sim time, so we can free up a thread
                let pool_index = self.task_to_pool[index];
                self.virtual_pools[pool_index].num_threads_occupied -= 1;
            }
        }
        self.step_count += 1;
        runnable_tasks
    }

    pub fn shutdown_callback_threads(&mut self) -> Result<(), Vec<usize>> {
        for sender in self.callback_exec_request_senders.drain(..) {
            // Best-effort: thread may already have exited if its sender was dropped.
            let _ = sender.send(CallbackExecutionRequest {
                index: 0,
                current_time: FrameworkTime::INVALID,
                should_run: false,
            });
        }

        let mut panicked_thread_indexes = vec![];
        for (thread_idx, t) in self.callback_executor_threads.drain(..).enumerate() {
            if t.join().is_err() {
                panicked_thread_indexes.push(thread_idx);
            }
        }

        if panicked_thread_indexes.is_empty() {
            Ok(())
        } else {
            Err(panicked_thread_indexes)
        }
    }

    pub fn get_step_count(&self) -> Saturating<usize> {
        self.step_count
    }

    pub fn get_simulation_time(&self) -> FrameworkTime {
        self.time
    }

    pub fn cleanup(&mut self) {
        // Clean up subscriber buffers so we can destroy publisher message storage
        for task in self.tasks.iter() {
            for subscriber in task.lock().unwrap().get_subscribers().iter() {
                subscriber.cleanup_buffers();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use task::test_tasks::*;

    use super::SimulationState;

    /// Lower level test that manually steps sim state
    #[test]
    fn test_simulation_state() {
        let (callbacks, task_info) = build_fizz_buzz_tasks();

        let mut state = SimulationState::new(1, callbacks);
        state.start();
        let start_time = state.get_simulation_time();

        assert_eq!(state.tasks.len(), 3);

        let periodic = state.periodic_tasks.front().unwrap();
        assert_eq!(periodic.index, 0);
        assert_eq!(periodic.requested_exec_time, start_time);

        let executed_tasks = state.step();
        assert_eq!(executed_tasks, vec![task_info.integer_publisher_index]);

        // After first step, the publisher should want to run in the future
        let periodic = state.periodic_tasks.front().unwrap();
        assert_eq!(periodic.index, 0);
        assert_eq!(
            periodic.requested_exec_time,
            start_time + Duration::from_millis(1) + Duration::from_millis(500),
            "Publisher task takes 1ms to run and wants to run every 500ms"
        );

        let executed_tasks = state.step();
        assert_eq!(
            executed_tasks,
            vec![task_info.fizz_buzz_index],
            "After the second step, the fizz-buzz task should have run"
        );

        let executed_tasks = state.step();
        assert_eq!(
            executed_tasks,
            vec![task_info.string_store_index],
            "After the third step, the string store task should've run"
        );

        assert_eq!(task_info.get_stored_strings(), vec!["FizzBuzz"]);
    }
}
