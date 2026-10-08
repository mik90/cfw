pub mod capture;
pub mod log_file;
#[cfg(feature = "serde")]
pub mod log_file_json;
pub mod replay_source;
pub mod session;
#[cfg(feature = "testing")]
pub mod testing;

pub use capture::{Capture, CapturePlan};
pub use replay_source::{ReplaySource, ReplaySourcePlan};
pub use session::{LogSession, LogStatus};
pub use task::recording::{ExecutionDescriptor, ExecutionRecord, ExecutionRecorder, ObservedEvent};
#[cfg(feature = "testing")]
pub use testing::InMemoryWriter;

pub use log_file::{
    BoxedLogError, LogEntry, LogEntryIter, LogFileReader, LogFileWriter, SharedLogFileWriter,
};
