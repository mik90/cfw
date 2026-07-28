use std::sync::atomic::Ordering;
use std::sync::Weak;
use task::executor::ExecutorStopSignal;

use crate::pool_state::SharedThreadPoolState;

pub struct StopSignal(pub(crate) Weak<SharedThreadPoolState>);

impl ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        let Some(state) = self.0.upgrade() else {
            return;
        };
        state.should_run.store(false, Ordering::Relaxed);
        state.periodic_cond_var.notify_all();
    }
}
