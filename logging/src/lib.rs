pub mod capture;
pub mod port_capture;
pub use port_capture::PortCapture;
pub mod log_file;
#[cfg(feature = "serde")]
pub mod log_file_json;
#[cfg(feature = "serde")]
pub mod replay_feed;
pub mod replay_source;
#[cfg(feature = "serde")]
pub use replay_feed::ReplayFeed;
pub mod session;
#[cfg(feature = "serde")]
pub mod sorted_log_stream;
#[cfg(feature = "serde")]
pub use sorted_log_stream::{LogReadError, OwnedLogEntry, SortedLogStreamReader};
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
