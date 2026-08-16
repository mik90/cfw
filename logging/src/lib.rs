pub mod log_build_step;
pub mod log_capture;
pub mod log_diagnostics_task;
pub mod log_file;
#[cfg(feature = "serde")]
pub mod log_file_json;
pub mod log_task;
#[cfg(feature = "serde")]
pub mod sorted_log_stream;

pub use log_build_step::{LogTaskConfiguration, LoggingBuildStep, LoggingStrategy};
pub use log_diagnostics_task::{DiagnosticsMode, LogDiagnosticsTask};
pub use log_file::{
    BoxedLogError, LogEntry, LogEntryIter, LogFileReader, LogFileWriter, SharedLogFileWriter,
};
pub use log_task::{LogError, log_task_diagnostics_channel, log_task_name};
#[cfg(feature = "serde")]
pub use sorted_log_stream::{
    OwnedLogEntry, ReplaySink, ReplaySinkMap, SortedLogStreamReader, build_replay_sinks,
};
// Re-export ChannelRegistry from `task` so users of the `logging` crate have
// a single import surface for logging-relevant types.
pub use task::channel_registry::ChannelRegistry;
