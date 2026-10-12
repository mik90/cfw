use task::{executor::TimeSource, time::FrameworkTime};

#[derive(Debug)]
pub struct WallClock;

impl TimeSource for WallClock {
    fn now(&self) -> FrameworkTime {
        FrameworkTime::from_wall_clock()
    }
}
