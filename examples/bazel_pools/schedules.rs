use std::time::Duration;

use task_graph::CallbackSchedule;

pub fn doubler() -> CallbackSchedule {
    CallbackSchedule::input_triggered().with_execution_duration(Duration::from_millis(1))
}
