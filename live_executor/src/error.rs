use task::LoanError;

#[derive(Debug)]
pub struct LiveExecutorStartError {
    pub reason: String,
}

#[derive(Debug)]
pub enum ThreadFailure {
    Timing {
        callback: String,
        source: task::TimingError,
    },
    Readiness {
        reason: String,
    },
    Callback {
        worker: usize,
        callback: String,
        source: LoanError,
    },
    Panic {
        thread: String,
    },
}

#[derive(Debug)]
pub enum LiveExecutorError {
    Start(LiveExecutorStartError),
    Threads(Vec<ThreadFailure>),
}

impl std::fmt::Display for LiveExecutorStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}
impl std::error::Error for LiveExecutorStartError {}
impl std::fmt::Display for LiveExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(error) => write!(f, "executor startup failed: {error}"),
            Self::Threads(failures) => write!(f, "executor threads failed: {failures:?}"),
        }
    }
}
impl std::error::Error for LiveExecutorError {}
