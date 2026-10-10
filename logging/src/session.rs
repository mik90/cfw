use crate::{BoxedLogError, Capture, LogFileWriter};
use std::sync::{Arc, Mutex};
use task::recording::ExecutionRecorder;

/// Observable errors from explicit flushing and best-effort destruction flushing.
#[derive(Clone, Default)]
pub struct LogStatus(Arc<Mutex<Vec<String>>>);
impl LogStatus {
    pub fn errors(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// Owns already-bound captures; the graph's storage must outlive this session.
/// Flush periodically while executing, then finish explicitly to return IO errors,
/// or inspect status after automatic normal/panic cleanup.
/// With serde enabled, known failures also write sticky incompleteness snapshots.
/// A failed sink can prevent persistence; this is not crash/truncation detection.
pub struct LogSession<'a> {
    captures: Vec<Capture<'a>>,
    writer: Box<dyn LogFileWriter>,
    recorder: Option<ExecutionRecorder>,
    status: LogStatus,
    finished: bool,
}
impl<'a> LogSession<'a> {
    pub fn new(writer: impl LogFileWriter + 'static, captures: Vec<Capture<'a>>) -> Self {
        Self {
            captures,
            writer: Box::new(writer),
            recorder: None,
            status: LogStatus::default(),
            finished: false,
        }
    }
    #[cfg(feature = "serde")]
    pub fn with_recording(mut self, recorder: ExecutionRecorder) -> Result<Self, BoxedLogError> {
        let channels = self
            .captures
            .iter()
            .map(|capture| capture.channel().to_owned())
            .collect();
        self.attach_recording(recorder, channels)?;
        Ok(self)
    }
    #[cfg(feature = "serde")]
    pub(crate) fn attach_recording(
        &mut self,
        recorder: ExecutionRecorder,
        channels: Vec<String>,
    ) -> Result<(), BoxedLogError> {
        let mut descriptor = recorder
            .descriptor()
            .ok_or("recorder must be attached before constructing the log session")?;
        descriptor.logged_channels = channels;
        descriptor.logged_channels.sort();
        descriptor.logged_channels.dedup();
        self.recorder = Some(recorder);
        if let Err(error) = self.writer.write_artifact(
            task::recording::EXECUTION_LOG_DESCRIPTOR_ARTIFACT,
            &serde_json::to_vec(&descriptor)?,
        ) {
            self.status
                .0
                .lock()
                .unwrap()
                .push(format!("execution descriptor: {error}"));
            return Err(error);
        }
        Ok(())
    }
    pub fn status(&self) -> LogStatus {
        self.status.clone()
    }
    fn flush_inner(&mut self) -> Result<(), BoxedLogError> {
        let mut first_error = None;
        for capture in &mut self.captures {
            let channel = capture.channel().to_owned();
            if let Err(error) =
                capture.drain(|header, body| self.writer.store_message(&channel, &header, body))
            {
                self.status
                    .0
                    .lock()
                    .unwrap()
                    .push(format!("capture '{channel}': {error}"));
                first_error.get_or_insert(error);
            }
        }
        #[cfg(feature = "serde")]
        if let Some(recorder) = &self.recorder {
            if recorder.dropped() != 0 {
                let error: BoxedLogError = format!(
                    "execution recording overflow: {} records dropped",
                    recorder.dropped()
                )
                .into();
                self.status.0.lock().unwrap().push(error.to_string());
                first_error.get_or_insert(error);
            }
            for record in recorder.drain() {
                let result = (|| {
                    self.writer.store_message(
                        task::recording::EXECUTION_LOG_CHANNEL,
                        &task::message::MessageHeader::new(record.execution_time),
                        &serde_json::to_vec(&record)?,
                    )
                })();
                if let Err(error) = result {
                    self.status
                        .0
                        .lock()
                        .unwrap()
                        .push(format!("execution record: {error}"));
                    first_error.get_or_insert(error);
                }
            }
            for event in recorder.drain_events() {
                let result = (|| {
                    self.writer.store_message(
                        task::recording::EXECUTION_EVENT_CHANNEL,
                        &task::message::MessageHeader::new(event.observed_at),
                        &serde_json::to_vec(&event)?,
                    )
                })();
                if let Err(error) = result {
                    self.status
                        .0
                        .lock()
                        .unwrap()
                        .push(format!("execution event: {error}"));
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
    #[cfg(feature = "serde")]
    fn persist_incompleteness(&mut self) -> Result<(), BoxedLogError> {
        let errors = self.status.errors();
        if errors.is_empty() {
            return Ok(());
        }
        let snapshot = crate::incompleteness::RecordingIncompleteness {
            captures: self.captures.iter().map(Capture::loss).collect(),
            recorder_entries_dropped: self.recorder.as_ref().map_or(0, ExecutionRecorder::dropped),
            errors,
        };
        self.writer.write_artifact(
            crate::incompleteness::RECORDING_INCOMPLETENESS_ARTIFACT,
            &serde_json::to_vec(&snapshot)?,
        )
    }
    pub fn flush(&mut self) -> Result<(), BoxedLogError> {
        let result = self.flush_inner();
        // Persist any successfully written prefix even when capture/serialization
        // fails; both failures remain observable through status.
        #[cfg(feature = "serde")]
        let result = {
            let marker = self.persist_incompleteness();
            if let Err(error) = &marker {
                self.status
                    .0
                    .lock()
                    .unwrap()
                    .push(format!("incompleteness artifact: {error}"));
            }
            result.and(marker)
        };
        let flushed = self.writer.flush();
        if let Err(error) = &flushed {
            self.status.0.lock().unwrap().push(error.to_string());
            // The sink may recover; attempt to mark a flush failure and flush
            // that marker once. Keep the original failure observable regardless.
            #[cfg(feature = "serde")]
            {
                if let Err(error) = self.persist_incompleteness() {
                    self.status
                        .0
                        .lock()
                        .unwrap()
                        .push(format!("incompleteness artifact: {error}"));
                }
                if let Err(error) = self.writer.flush() {
                    self.status.0.lock().unwrap().push(error.to_string());
                }
            }
        }
        result.and(flushed)
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
            && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.flush())).is_err()
        {
            self.status
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push("log writer panicked during final flush".into());
        }
    }
}
