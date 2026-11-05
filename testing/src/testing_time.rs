use std::sync::RwLock;

use task::time::FrameworkTime;

/// Thread-safe time source so that we can access time across multiple test threads.
/// Time will only ever be updated by the simulation executor between steps, although it'll be read often
/// by code within each step.
// We could use an atomic here, although reasoning about which ordering to set isn't worth the effort.
// A RwLock is probably best since it allows multiple readers concurrently. I really don't see much issue
// with contention on a normal mutex though.
pub struct TimeSource(RwLock<FrameworkTime>);

impl TimeSource {
    pub fn new(time: FrameworkTime) -> Self {
        Self(RwLock::new(time))
    }

    pub fn get(&self) -> FrameworkTime {
        *self.0.read().unwrap()
    }

    pub fn set(&self, time: FrameworkTime) {
        *self.0.write().unwrap() = time;
    }
}
