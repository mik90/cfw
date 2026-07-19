use task::callback::{Callback, Run};
use task::context::Context;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::input::OptionalInput;
use task::subscriber::SubscriberConfig;

use crate::log_task::{LOG_TASK_DIAGNOSTICS_CHANNEL, LogError};

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

impl Callback for LogDiagnosticsTask {
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn GenericSubscriber>],
        _publishers: &mut [Box<dyn GenericPublisher>],
        ctx: &Context,
    ) -> Run {
        if let Some(sub) = subscribers.first_mut() {
            let input: OptionalInput<'_, LogError> = OptionalInput::new_downcasted(sub.as_mut());
            if let Some(err) = input.value() {
                self.error_count += 1;
                let err_clone = LogError {
                    channel: err.channel.clone(),
                    message: err.message.clone(),
                    at: err.at,
                };
                drop(input);
                match self.mode {
                    DiagnosticsMode::Print => {
                        eprintln!(
                            "log_task_diagnostics @ {}: channel='{}' error='{}'",
                            ctx.now, err_clone.channel, err_clone.message
                        );
                    }
                    DiagnosticsMode::Panic => {
                        panic!(
                            "log_task_diagnostics @ {}: channel='{}' error='{}'",
                            ctx.now, err_clone.channel, err_clone.message
                        );
                    }
                    DiagnosticsMode::Silent => {}
                }
            }
        }
        Run::new(1)
    }

    fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
        vec![Box::new(task::subscriber::Subscriber::<LogError>::new(
            SubscriberConfig {
                // Optional so the diagnostic task runs even with no errors;
                // triggering once a new error arrives flushes it before the
                // next keep_across_runs cycle.
                is_optional: true,
                capacity: 1,
                is_trigger: true,
                keep_across_runs: true,
                channel_name: LOG_TASK_DIAGNOSTICS_CHANNEL.to_string(),
            },
        ))]
    }

    fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
        vec![]
    }
}
