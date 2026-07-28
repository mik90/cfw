use std::fmt;

#[derive(Debug)]
pub struct LiveExecutorError {
    pub panicked_thread_indices: Vec<usize>,
}

impl fmt::Display for LiveExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "threads panicked: {:?}", self.panicked_thread_indices)
    }
}

impl std::error::Error for LiveExecutorError {}
