use std::time::Instant;

#[derive(Clone)]
pub struct Context {
    pub now: Instant,
}

impl Context {
    pub fn new(now: Instant) -> Self {
        Context { now }
    }
}
