//! Inspectable failures independent of reporting policy. Print writes to stderr;
//! Panic is applied after best-effort persistence/flush and suppressed in Drop.
use std::sync::{Arc, Mutex};
use task::{message::MessageHeader, time::FrameworkTime};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiagnosticPolicy {
    #[default]
    Silent,
    Print,
    Panic,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DiagnosticKind {
    Capture,
    Serialization,
    Write,
    Overflow,
    Receive,
    Descriptor,
    Artifact,
    Flush,
    Panic,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LogDiagnostic {
    pub channel: Option<String>,
    /// Executor time supplied to flush_at; absent for manual/final cleanup.
    pub at: Option<FrameworkTime>,
    pub header: Option<MessageHeader>,
    pub kind: DiagnosticKind,
    pub error: String,
}
impl std::fmt::Display for LogDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} channel={:?} at={:?}: {}",
            self.kind, self.channel, self.at, self.error
        )
    }
}
#[derive(Clone, Default)]
pub struct LogStatus(Arc<Mutex<Vec<LogDiagnostic>>>);
impl LogStatus {
    pub fn diagnostics(&self) -> Vec<LogDiagnostic> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub fn errors(&self) -> Vec<String> {
        self.diagnostics().iter().map(ToString::to_string).collect()
    }
    pub(crate) fn push(&self, diagnostic: LogDiagnostic) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(diagnostic);
    }
}
