pub mod log_build_step;
pub mod log_diagnostics_task;
pub mod log_file;
#[cfg(feature = "serde")]
pub mod log_file_json;
pub mod log_task;

pub use log_build_step::LoggingBuildStep;
pub use log_diagnostics_task::{DiagnosticsMode, LogDiagnosticsTask};
pub use log_file::{
    BoxedLogError, LogEntry, LogEntryIter, LogFileReader, LogFileWriter, SharedLogFileWriter,
};
pub use log_task::{LogError, LogTaskConfiguration, log_task_diagnostics_channel, log_task_name};
// Re-export ChannelRegistry from `task` so users of the `logging` crate have
// a single import surface for logging-relevant types.
pub use task::channel_registry::ChannelRegistry;
