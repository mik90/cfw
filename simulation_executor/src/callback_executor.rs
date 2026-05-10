use crate::{ConnectedCallback, Context, FrameworkTime, Run, TaskIndex};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) struct CallbackExecutionRequest {
    /// Index of task to execute
    pub index: TaskIndex,
    /// Current simulation time
    pub current_time: FrameworkTime,
    /// Whether execution should continue. False signals the thread to exit.
    pub should_run: bool,
}

#[derive(Debug)]
pub(crate) struct CallbackExecutionResponse {
    /// Task index that was executed
    pub index: TaskIndex,
    /// How long the task took in simulation
    pub execution_duration: Duration,
    /// Run result
    pub run_result: Run,
}

/// Runs sim callbacks when work is provided
pub(crate) fn callback_executor_thread(
    work_receiver: Receiver<CallbackExecutionRequest>,
    response_sender: Sender<CallbackExecutionResponse>,
    tasks: Vec<Arc<Mutex<ConnectedCallback>>>,
) {
    loop {
        let work_request = match work_receiver.recv() {
            Ok(req) => req,
            // Senders dropped: treat as clean exit
            Err(_) => return,
        };
        if !work_request.should_run {
            return;
        }

        let ctx = Context::new(work_request.current_time);
        let task = &mut tasks[work_request.index].lock().unwrap();
        let result = task.run(&ctx);

        let response = CallbackExecutionResponse {
            index: work_request.index,
            execution_duration: task.get_execution_duration(),
            run_result: result,
        };
        if let Err(e) = response_sender.send(response) {
            panic!("Could not respond to execution request: {}", e);
        }
    }
}
