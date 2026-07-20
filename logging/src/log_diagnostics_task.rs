use task::callback::{Callback, Run};
use task::context::Context;
use task::generic_publisher::GenericPublisher;
use task::generic_subscriber::GenericSubscriber;
use task::pub_sub::ChannelName;
use task::subscriber::{Subscriber, SubscriberConfig};

use crate::log_task::{LogError, log_task_diagnostics_channel};

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

/// A `Callback` that subscribes to one `LogError` diagnostics channel per
/// `LogTask` and reacts to errors they produce. Designed to be added to a
/// `TaskGraph` alongside the `LoggingBuildStep` — the `LogTask`s produce,
/// this consumes.
pub struct LogDiagnosticsTask {
    mode: DiagnosticsMode,
    /// Total errors observed since construction. Inspection-only.
    error_count: usize,
    /// One subscriber is built per channel — typically one per `LogTask`.
    channel_names: Vec<ChannelName>,
}

impl LogDiagnosticsTask {
    pub fn new(mode: DiagnosticsMode, channel_names: Vec<ChannelName>) -> Self {
        LogDiagnosticsTask {
            mode,
            error_count: 0,
            channel_names,
        }
    }

    /// Subscribe to the diagnostics channels of the first `num_log_tasks`
    /// `LogTask`s produced by a `LoggingBuildStep`.
    pub fn for_log_tasks(mode: DiagnosticsMode, num_log_tasks: usize) -> Self {
        Self::new(
            mode,
            (0..num_log_tasks)
                .map(log_task_diagnostics_channel)
                .collect(),
        )
    }

    /// How many errors have been observed so far.
    pub fn error_count(&self) -> usize {
        self.error_count
    }

    fn react(&mut self, err: &LogError) {
        self.error_count += 1;
        match self.mode {
            DiagnosticsMode::Print => {
                eprintln!(
                    "log_task_diagnostics: channel='{}' error='{}'",
                    err.channel, err.message
                );
            }
            DiagnosticsMode::Panic => {
                panic!(
                    "log_task_diagnostics: channel='{}' error='{}'",
                    err.channel, err.message
                );
            }
            DiagnosticsMode::Silent => {}
        }
    }
}

impl Callback for LogDiagnosticsTask {
    fn run_generic(
        &mut self,
        subscribers: &mut [Box<dyn GenericSubscriber>],
        _publishers: &mut [Box<dyn GenericPublisher>],
        _ctx: &Context,
    ) -> Run {
        for subscriber in subscribers.iter_mut() {
            let typed = subscriber
                .as_any()
                .downcast_mut::<Subscriber<LogError>>()
                .expect("LogDiagnosticsTask subscribers are built as Subscriber<LogError>");
            // Consume the front error (capacity is 1, so that's the whole
            // queue) and pop it so it isn't reprocessed on the next run —
            // the subscriber uses `keep_across_runs: true`.
            let mut guard = typed.read_buffer();
            if let Some(message) = guard.front() {
                self.react(&message.message);
                guard.pop_front();
            }
        }
        Run::new(1)
    }

    fn build_subscribers(&self) -> Vec<Box<dyn GenericSubscriber>> {
        self.channel_names
            .iter()
            .map(|channel_name| {
                Box::new(Subscriber::<LogError>::new(SubscriberConfig {
                    is_optional: true,
                    capacity: 1,
                    is_trigger: true,
                    keep_across_runs: true,
                    channel_name: channel_name.clone(),
                })) as Box<dyn GenericSubscriber>
            })
            .collect()
    }

    fn build_publishers(&self) -> Vec<Box<dyn GenericPublisher>> {
        vec![]
    }
}
