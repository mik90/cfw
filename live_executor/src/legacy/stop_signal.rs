use std::sync::Weak;
use task::executor::ExecutorStopSignal;

use crate::pool_state::SharedThreadPoolState;

pub struct StopSignal(pub(crate) Weak<SharedThreadPoolState>);

impl ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        let Some(state) = self.0.upgrade() else {
            return;
        };
        state.request_stop();
    }
}
