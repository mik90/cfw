use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use task::callback::ConnectedCallback;
use task::executor::{Executor, ExecutorStopSignal};

pub struct StopSignal(Arc<AtomicBool>);

#[derive(Debug)]
pub struct LiveReplayExecutorError {
    pub panicked_thread_indices: Vec<usize>,
}

impl std::fmt::Display for LiveReplayExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "threads panicked: {:?}", self.panicked_thread_indices)
    }
}

impl std::error::Error for LiveReplayExecutorError {}

#[allow(dead_code)]
pub struct LiveReplayExecutor {
    // All tasks, regardless of if they're run or not
    tasks: Vec<ConnectedCallback>,

    // Threads where execution is run off of
    execution_threads: Vec<thread::JoinHandle<()>>,

    // Other threads may swap this on/off to stop
    should_run: Arc<AtomicBool>,
}

pub struct LiveReplayConfig {
    /// Speed at which logged messages should be replayed
    pub replay_speed: f32,
    // TODO: some way to load log files
    // TODO: some thread pool configuration
}

impl ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Executor for LiveReplayExecutor {
    type Error = LiveReplayExecutorError;

    fn start(&mut self) {
        self.should_run.store(true, Ordering::Release);
        let should_run = self.should_run.clone();
        self.execution_threads.push(thread::spawn(move || {
            while should_run.load(Ordering::Acquire) {
                todo!("Do something")
            }
        }));
    }

    fn stop(&mut self) -> Result<(), LiveReplayExecutorError> {
        self.should_run.store(false, Ordering::Release);

        let mut panicked_thread_indices = vec![];
        println!("Joining {} threads", self.execution_threads.len());
        for (thread_idx, t) in self.execution_threads.drain(..).enumerate() {
            if t.join().is_err() {
                panicked_thread_indices.push(thread_idx);
            }
            println!("joined thread {thread_idx}");
        }

        if panicked_thread_indices.is_empty() {
            Ok(())
        } else {
            Err(LiveReplayExecutorError { panicked_thread_indices })
        }
    }

    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal> {
        Arc::new(StopSignal(self.should_run.clone()))
    }

    fn is_running(&self) -> bool {
        self.should_run.load(Ordering::Acquire)
    }
}

impl LiveReplayExecutor {
    pub fn new(tasks: Vec<ConnectedCallback>) -> Self {
        Self {
            tasks,
            execution_threads: Vec::new(),
            should_run: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[cfg(test)]
mod tests {}
