use crate::time::FrameworkTime;

pub trait ExecutorStopSignal: Send + Sync {
    fn request_stop(&self);
}

pub trait TimeSource: Send + Sync {
    fn now(&self) -> FrameworkTime;
}

#[derive(Debug)]
pub struct WallClock;

impl TimeSource for WallClock {
    fn now(&self) -> FrameworkTime {
        FrameworkTime::from_wall_clock()
    }
}
