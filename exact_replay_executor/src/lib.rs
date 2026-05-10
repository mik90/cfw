use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use task::callback::ConnectedCallback;
use task::executor::{Executor, ExecutorError, ExecutorStopSignal};

pub struct StopSignal(Arc<AtomicBool>);

#[allow(dead_code)]
pub struct ExactReplayExecutor {
    // All tasks, regardless of if they're run or not
    tasks: Vec<ConnectedCallback>,

    // Threads where execution is run off of
    execution_threads: Vec<thread::JoinHandle<()>>,

    // Other threads may swap this on/off to stop
    should_run: Arc<AtomicBool>,
    //
    // TODO some way to request replay of a given execution timestamp, or a given message timestamp
    //
}

impl ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        self.0.store(false, Ordering::Release);
    }
}
impl Executor for ExactReplayExecutor {
    fn start(&mut self) {
        self.should_run.store(true, Ordering::Release);
        let should_run = self.should_run.clone();
        self.execution_threads.push(thread::spawn(move || {
            while should_run.load(Ordering::Acquire) {
                todo!("Do something")
            }
        }));
    }

    fn stop(&mut self) -> Result<(), ExecutorError> {
        self.should_run.store(false, Ordering::Release);

        let mut thread_join_result = vec![];
        println!("Joining {} threads", self.execution_threads.len());
        for (thread_idx, t) in self.execution_threads.drain(..).enumerate() {
            match t.join() {
                Ok(()) => {}
                Err(_) => {
                    thread_join_result.push(thread_idx);
                }
            }
            println!("joined thread {thread_idx}");
        }

        if thread_join_result.is_empty() {
            return Ok(());
        }
        Err(ExecutorError::PanickedThreads(thread_join_result))
    }

    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal> {
        Arc::new(StopSignal(self.should_run.clone()))
    }

    fn is_running(&self) -> bool {
        self.should_run.load(Ordering::Acquire)
    }
}

impl ExactReplayExecutor {
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
