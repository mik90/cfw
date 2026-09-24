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

/// Failure while collecting registrations or creating the event-readiness
/// shutdown resources before executor worker threads start.
#[derive(Debug)]
pub struct LiveExecutorStartError {
    /// Callback node whose event registration failed, when applicable.
    pub node: Option<String>,
    /// Underlying registration or service error.
    pub reason: String,
}

impl fmt::Display for LiveExecutorStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.node {
            Some(node) => write!(
                f,
                "unable to start iox2 readiness for {node}: {}",
                self.reason
            ),
            None => write!(f, "unable to start iox2 readiness: {}", self.reason),
        }
    }
}

impl std::error::Error for LiveExecutorStartError {}
