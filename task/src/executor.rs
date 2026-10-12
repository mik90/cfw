use crate::time::FrameworkTime;

pub trait ExecutorStopSignal: Send + Sync {
    fn request_stop(&self);
}

pub trait TimeSource: Send + Sync {
    fn now(&self) -> FrameworkTime;
}
