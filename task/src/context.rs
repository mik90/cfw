use crate::time::FrameworkTime;

#[derive(Clone)]
pub struct Context {
    pub now: FrameworkTime,
}

impl Context {
    pub fn new(now: FrameworkTime) -> Self {
        Context { now }
    }
}
