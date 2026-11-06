pub use crate::diagnostics::LogStatus;
use crate::{
    BoxedLogError, Capture, DiagnosticKind, DiagnosticPolicy, LogDiagnostic, LogFileWriter,
};
use task::time::FrameworkTime;
#[cfg(feature = "serde")]
use task_graph::execution_log::ExecutionLogMetadata;

/// Storage must outlive the session. Known failures are sticky and, with serde,
/// persisted best-effort. Final cleanup never propagates a diagnostic-policy panic.
pub struct LogSession<'a> {
    captures: Vec<Capture<'a>>,
    writer: Box<dyn LogFileWriter>,
    #[cfg(feature = "serde")]
    execution_log: Option<ExecutionLogMetadata>,
    status: LogStatus,
    policy: DiagnosticPolicy,
    finished: bool,
}
impl<'a> LogSession<'a> {
    pub fn new(writer: impl LogFileWriter + 'static, captures: Vec<Capture<'a>>) -> Self {
        let registry = crate::intern_tables::Registry::new(captures.iter().map(Capture::channel));
        Self::with_registry(writer, captures, registry)
    }
    pub(crate) fn with_registry(
        writer: impl LogFileWriter + 'static,
        captures: Vec<Capture<'a>>,
        registry: crate::intern_tables::Registry,
    ) -> Self {
        Self {
            captures,
            writer: Box::new(writer),
            #[cfg(feature = "serde")]
            execution_log: None,
            status: LogStatus::new(registry),
            policy: DiagnosticPolicy::Silent,
            finished: false,
        }
    }
    pub fn with_diagnostic_policy(mut self, policy: DiagnosticPolicy) -> Self {
        self.policy = policy;
        self
    }
    pub(crate) fn set_diagnostic_policy(&mut self, policy: DiagnosticPolicy) {
        self.policy = policy;
    }
    /// Attach execution-log metadata without starting the session.
    #[cfg(feature = "serde")]
    pub(crate) fn attach_execution_log(
        &mut self,
        metadata: ExecutionLogMetadata,
    ) -> Result<(), BoxedLogError> {
        let channels = self
            .captures
            .iter()
            .map(|c| task::ChannelName::new(c.channel()))
            .collect();
        self.attach_execution_log_for(metadata, channels)
    }
    #[cfg(feature = "serde")]
    pub(crate) fn attach_execution_log_for(
        &mut self,
        metadata: ExecutionLogMetadata,
        mut channels: Vec<task::ChannelName>,
    ) -> Result<(), BoxedLogError> {
        let mut descriptor = metadata.descriptor.clone();
        channels.retain(|channel| {
            channel.as_str() != task::execution_log_schema::EXECUTION_LOG_CHANNEL
                && channel.as_str() != task::execution_log_schema::IOX2_EVENT_RECORD_CHANNEL
        });
        descriptor.logged_channels = channels;
        descriptor.logged_channels.sort();
        descriptor.logged_channels.dedup();
        self.status
            .registry
            .configure(metadata.intern_tables.clone())?;
        self.status.registry.recording(descriptor)?;
        self.execution_log = Some(metadata);
        Ok(())
    }
    /// Attach execution-log metadata. Records arrive through captures; this only
    /// supplies the descriptor and drop counters.
    #[cfg(feature = "serde")]
    pub fn with_execution_log(
        mut self,
        metadata: ExecutionLogMetadata,
    ) -> Result<Self, BoxedLogError> {
        self.attach_execution_log(metadata)?;
        self.start()?;
        Ok(self)
    }
    /// Supply graph tables before starting a standalone session.
    pub fn with_intern_tables(self, tables: crate::InternTables) -> Result<Self, BoxedLogError> {
        self.status.registry.configure(tables)?;
        Ok(self)
    }
    /// Write intern tables first, then the execution descriptor when present.
    /// Idempotent. Capture-only sessions can call it explicitly or start on their
    /// first flush.
    pub fn start(&mut self) -> Result<(), BoxedLogError> {
        if let Err(error) = self.status.registry.start(self.writer.as_mut()) {
            let start = self.status.diagnostics().len();
            self.report(
                DiagnosticKind::Artifact,
                None,
                None,
                format!("logging startup artifacts: {error}"),
            );
            let _ = self.flush_impl(None, false);
            self.apply_policy(start, true);
            return Err(error);
        }
        Ok(())
    }
    pub fn status(&self) -> LogStatus {
        self.status.clone()
    }
    fn report(
        &self,
        kind: DiagnosticKind,
        at: Option<FrameworkTime>,
        channel: Option<&str>,
        error: impl ToString,
    ) {
        self.status.push(LogDiagnostic {
            kind,
            at,
            channel: channel.map(|name| self.status.registry.channel(name)),
            header: None,
            error: error.to_string(),
        });
    }
    fn flush_inner(&mut self, at: Option<FrameworkTime>) -> Result<(), BoxedLogError> {
        self.status
            .registry
            .start(self.writer.as_mut())
            .inspect_err(|error| self.report(DiagnosticKind::Artifact, at, None, error))?;
        let mut first = None;
        for capture in &mut self.captures {
            let channel = capture.channel().to_owned();
            if let Err(error) =
                capture.drain(|header, body| self.writer.store_message(&channel, &header, body))
            {
                use task_graph::capture::{CaptureFailure, CaptureFailureKind};
                let (kind, header) = error
                    .downcast_ref::<CaptureFailure>()
                    .map(|e| {
                        (
                            match e.kind {
                                CaptureFailureKind::Overflow => DiagnosticKind::Overflow,
                                CaptureFailureKind::Serialization => DiagnosticKind::Serialization,
                                CaptureFailureKind::Write => DiagnosticKind::Write,
                                CaptureFailureKind::Receive => DiagnosticKind::Receive,
                            },
                            e.header,
                        )
                    })
                    .unwrap_or((DiagnosticKind::Capture, None));
                self.status.push(LogDiagnostic {
                    kind,
                    channel: Some(self.status.registry.channel(&channel)),
                    at,
                    header,
                    error: error.to_string(),
                });
                first.get_or_insert(error);
            }
        }
        #[cfg(feature = "serde")]
        if let Some(metadata) = &self.execution_log {
            if metadata.dropped_records() != 0 {
                let error: BoxedLogError = format!(
                    "execution log overflow: {} records dropped",
                    metadata.dropped_records()
                )
                .into();
                self.report(
                    DiagnosticKind::Overflow,
                    at,
                    Some(task::execution_log_schema::EXECUTION_LOG_CHANNEL),
                    &error,
                );
                first.get_or_insert(error);
            }
            if metadata.dropped_iox2_event_records() != 0 {
                let error: BoxedLogError = format!(
                    "iox2 event record overflow: {} records dropped",
                    metadata.dropped_iox2_event_records()
                )
                .into();
                self.report(
                    DiagnosticKind::Overflow,
                    at,
                    Some(task::execution_log_schema::IOX2_EVENT_RECORD_CHANNEL),
                    &error,
                );
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }
    #[cfg(feature = "serde")]
    fn persist_incompleteness(&mut self) -> Result<(), BoxedLogError> {
        let errors = self.status.errors();
        if errors.is_empty() {
            return Ok(());
        }
        self.status.registry.start(self.writer.as_mut())?;
        let snapshot = crate::incompleteness::RecordingIncompleteness {
            captures: self.captures.iter().map(Capture::loss).collect(),
            execution_log_entries_dropped: self
                .execution_log
                .as_ref()
                .map_or(0, ExecutionLogMetadata::dropped),
            errors,
            diagnostics: self.status.diagnostics(),
        };
        self.writer.write_artifact(
            crate::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT,
            &serde_json::to_vec(&snapshot)?,
        )
    }
    pub fn flush(&mut self) -> Result<(), BoxedLogError> {
        self.flush_impl(None, true)
    }
    pub fn flush_at(&mut self, at: FrameworkTime) -> Result<(), BoxedLogError> {
        self.flush_impl(Some(at), true)
    }
    fn flush_impl(
        &mut self,
        at: Option<FrameworkTime>,
        allow_panic: bool,
    ) -> Result<(), BoxedLogError> {
        let start = self.status.diagnostics().len();
        let result = self.flush_inner(at);
        #[cfg(feature = "serde")]
        let result = {
            let marker = self.persist_incompleteness();
            if let Err(error) = &marker {
                self.report(DiagnosticKind::Artifact, at, None, error);
            }
            result.and(marker)
        };
        let flushed = self.writer.flush();
        if let Err(error) = &flushed {
            self.report(DiagnosticKind::Flush, at, None, error);
            #[cfg(feature = "serde")]
            {
                if let Err(error) = self.persist_incompleteness() {
                    self.report(DiagnosticKind::Artifact, at, None, error);
                }
                if let Err(error) = self.writer.flush() {
                    self.report(DiagnosticKind::Flush, at, None, error);
                }
            }
        }
        self.apply_policy(start, allow_panic);
        result.and(flushed)
    }
    fn apply_policy(&self, start: usize, allow_panic: bool) {
        let diagnostics = self.status.diagnostics();
        let new = &diagnostics[start..];
        match self.policy {
            DiagnosticPolicy::Print => {
                use std::io::Write;
                for diagnostic in new {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "{}",
                        self.status.format(diagnostic)
                    );
                }
            }
            DiagnosticPolicy::Panic
                if allow_panic && !std::thread::panicking() && !new.is_empty() =>
            {
                panic!("logging failure: {}", self.status.format(&new[0]))
            }
            _ => {}
        }
    }
    pub fn finish(mut self) -> Result<(), BoxedLogError> {
        self.finished = true;
        self.flush()?;
        if let Some(error) = self.status.errors().first() {
            return Err(format!("log session is incomplete: {error}").into());
        }
        Ok(())
    }
}
impl Drop for LogSession<'_> {
    fn drop(&mut self) {
        if !self.finished
            && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.flush_impl(None, false)
            }))
            .is_err()
        {
            self.report(
                DiagnosticKind::Panic,
                None,
                None,
                "log writer panicked during final flush",
            );
        }
    }
}
