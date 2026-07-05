use crate::{CallbackNode, CallbackNodeIndex, Context, FrameworkTime};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) struct NodeExecutionRequest {
    /// Index of callback node to execute
    pub index: CallbackNodeIndex,
    /// Current simulation time
    pub current_time: FrameworkTime,
    /// Whether execution should continue. False signals the thread to exit.
    pub should_run: bool,
}

#[derive(Debug)]
pub(crate) struct NodeExecutionResponse {
    /// Callback node index that was executed
    pub index: CallbackNodeIndex,
    /// How long the callback node took in simulation
    pub execution_duration: Duration,
}

/// Runs sim callback nodes when work is provided
pub(crate) fn node_executor_thread(
    work_receiver: Receiver<NodeExecutionRequest>,
    response_sender: Sender<NodeExecutionResponse>,
    nodes: Vec<Arc<Mutex<CallbackNode>>>,
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
        let node = &mut nodes[work_request.index].lock().unwrap();
        let _ = node.run(&ctx);

        let response = NodeExecutionResponse {
            index: work_request.index,
            execution_duration: node.get_execution_duration(),
        };
        // If the receiver is gone the step thread has exited; nothing left to do.
        if response_sender.send(response).is_err() {
            return;
        }
    }
}
