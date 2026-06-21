use std::sync::Mutex;

use crate::time::FrameworkTime;

pub struct TimeSource(Mutex<FrameworkTime>);

impl TimeSource {
    pub fn new(time: FrameworkTime) -> Self {
        Self(Mutex::new(time))
    }

    pub fn get(&self) -> FrameworkTime {
        *self.0.lock().unwrap()
    }

    pub fn set(&self, time: FrameworkTime) {
        *self.0.lock().unwrap() = time;
    }
}
