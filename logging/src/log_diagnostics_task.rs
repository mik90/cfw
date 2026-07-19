use task::input::OptionalInput;
use task_macros::task_callback;

use crate::log_task::LogError;

/// What the diagnostics task does when it observes a `LogError`.
#[derive(Clone, Copy, Debug)]
pub enum DiagnosticsMode {
    /// Print every error to stderr.
    Print,
    /// Panic on the first error — useful for tests that should hard-fail.
    Panic,
    /// Count but don't emit anything (default — keeps production quiet).
    Silent,
}

/// A `Callback` that subscribes to `log_task_diagnostics` and reacts to
/// errors produced by the `LogTask`. Designed to be added to a `TaskGraph`
/// alongside the `LoggingBuildStep` — the `LogTask` produces, this consumes.
pub struct LogDiagnosticsTask {
    mode: DiagnosticsMode,
    /// Total errors observed since construction. Inspection-only.
    error_count: usize,
}

impl LogDiagnosticsTask {
    pub fn new(mode: DiagnosticsMode) -> Self {
        LogDiagnosticsTask {
            mode,
            error_count: 0,
        }
    }

    /// How many errors have been observed so far.
    pub fn error_count(&self) -> usize {
        self.error_count
    }
}

#[task_callback]
impl LogDiagnosticsTask {
    fn run(&mut self, mut input: OptionalInput<LogError>) {
        // `OptionalInput::value()` returns a borrow tied to the input's
        // lifetime, so we clone out the fields via `.map()` (which produces
        // owned Strings and ends the borrow) before calling `input.clear()`.
        let err_data = input
            .value()
            .map(|err| (err.channel.clone(), err.message.clone()));
        if let Some((channel, message)) = err_data {
            self.error_count += 1;
            match self.mode {
                DiagnosticsMode::Print => {
                    eprintln!(
                        "log_task_diagnostics: channel='{}' error='{}'",
                        channel, message
                    );
                }
                DiagnosticsMode::Panic => {
                    panic!(
                        "log_task_diagnostics: channel='{}' error='{}'",
                        channel, message
                    );
                }
                DiagnosticsMode::Silent => {}
            }
            // Pop the consumed error so it isn't reprocessed on the next run
            // (LogDiagnosticsTask's subscriber uses `keep_across_runs: true`,
            // as the macro-generated `build_subscribers` defaults to).
            input.clear();
        }
    }
}

/// Public channel name constant for users wiring their own subscriber
/// (the macro-generated `build_subscribers` produces a `Subscriber<LogError>`
/// whose channel name is empty — set it via `CallbackBuilder::with_subscriber_channels`).
pub use crate::log_task::LOG_TASK_DIAGNOSTICS_CHANNEL as DIAGNOSTICS_CHANNEL;
