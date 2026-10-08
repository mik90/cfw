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
        let mut descriptor = recorder
            .descriptor()
            .ok_or("recorder must be attached before constructing the log session")?;
        descriptor.logged_channels = self
            .captures
            .iter()
            .map(|capture| capture.channel().to_owned())
            .collect();
        descriptor.logged_channels.sort();
        descriptor.logged_channels.dedup();
        self.writer.write_artifact(
            task::recording::EXECUTION_LOG_DESCRIPTOR_ARTIFACT,
            &serde_json::to_vec(&descriptor)?,
        )?;
        self.recorder = Some(recorder);
        Ok(self)
    }
    pub fn status(&self) -> LogStatus {
        self.status.clone()
    }
    fn flush_inner(&mut self) -> Result<(), BoxedLogError> {
        for capture in &mut self.captures {
            let channel = capture.channel().to_owned();
            capture.drain(|header, body| self.writer.store_message(&channel, &header, body))?;
        }
        #[cfg(feature = "serde")]
        if let Some(recorder) = &self.recorder {
            if recorder.dropped() != 0 {
                return Err(format!(
                    "execution recording overflow: {} records dropped",
                    recorder.dropped()
                )
                .into());
            }
            for record in recorder.drain() {
                self.writer.store_message(
                    task::recording::EXECUTION_LOG_CHANNEL,
                    &task::message::MessageHeader::new(record.execution_time),
                    &serde_json::to_vec(&record)?,
                )?;
            }
            for event in recorder.drain_events() {
                self.writer.store_message(
                    task::recording::EXECUTION_EVENT_CHANNEL,
                    &task::message::MessageHeader::new(event.observed_at),
                    &serde_json::to_vec(&event)?,
                )?;
            }
        }
        Ok(())
    }
    pub fn flush(&mut self) -> Result<(), BoxedLogError> {
        let result = self.flush_inner();
        // Persist any successfully written prefix even when capture/serialization
        // fails; both failures remain observable through status.
        if let Err(error) = &result {
            self.status.0.lock().unwrap().push(error.to_string());
        }
        let flushed = self.writer.flush();
        if let Err(error) = &flushed {
            self.status.0.lock().unwrap().push(error.to_string());
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
