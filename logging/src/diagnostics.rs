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
    pub channel: Option<task::string_interner::ChannelId>,
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
pub struct LogStatus {
    diagnostics: Arc<Mutex<Vec<LogDiagnostic>>>,
    pub(crate) registry: crate::intern_tables::Registry,
}
impl LogStatus {
    pub(crate) fn new(registry: crate::intern_tables::Registry) -> Self {
        Self {
            registry,
            diagnostics: Default::default(),
        }
    }
    pub fn intern_tables(&self) -> crate::InternTables {
        self.registry.tables()
    }
    pub fn format(&self, diagnostic: &LogDiagnostic) -> String {
        let tables = self.intern_tables();
        let channel = diagnostic
            .channel
            .and_then(|id| tables.channels.try_lookup_by_id(id));
        format!(
            "{:?} channel={channel:?} at={:?}: {}",
            diagnostic.kind, diagnostic.at, diagnostic.error
        )
    }
    pub fn diagnostics(&self) -> Vec<LogDiagnostic> {
        self.diagnostics
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn errors(&self) -> Vec<String> {
        self.diagnostics()
            .iter()
            .map(|diagnostic| self.format(diagnostic))
            .collect()
    }
    pub(crate) fn push(&self, diagnostic: LogDiagnostic) {
        self.diagnostics
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(diagnostic);
    }
}
