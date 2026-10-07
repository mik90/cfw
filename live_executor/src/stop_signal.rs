use crate::pool_state::Scheduler;
use crossbeam::channel::Receiver;
use std::sync::Arc;

/// May outlive the execution scope: it references scheduling metadata only.
#[derive(Clone)]
pub struct StopSignal(pub(crate) Arc<Scheduler>);

impl StopSignal {
    pub fn request_stop(&self) {
        self.0.request_stop();
    }
    pub fn is_stopped(&self) -> bool {
        self.0.is_stopped()
    }
    /// Block until shutdown is requested, including a worker failure.
    pub fn wait(&self) {
        let _ = self.0.stop_rx.recv();
    }
    /// Disconnection signals shutdown to every receiver. A main-thread signal
    /// loop can select on this alongside its OS-signal/event receiver.
    pub fn receiver(&self) -> Receiver<()> {
        self.0.stop_rx.clone()
    }
}

impl task::executor::ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        StopSignal::request_stop(self);
    }
}
