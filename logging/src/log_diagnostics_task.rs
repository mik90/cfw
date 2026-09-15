use task::callback::{Callback, PubOrSub, PubOrSubMut};
use task::context::Context;
use task::input::OptionalInput;
use task::pub_sub::ChannelName;
use task::subscriber::{Subscriber, SubscriberConfig};

use crate::log_task::LogError;

pub(crate) const DEFAULT_DIAGNOSTICS_TASK_NAME: &str = "log_diagnostics";

#[derive(Clone, Copy, PartialEq)]
pub enum DiagnosticsMode {
    Print,
    Panic,
    Silent,
}

pub struct LogDiagnosticsTask {
    channel_names: Vec<ChannelName>,
    subscribers: Vec<Subscriber<LogError>>,
    error_count: u64,
    mode: DiagnosticsMode,
}

impl LogDiagnosticsTask {
    /// Create a `LogDiagnosticsTask` that subscribes to diagnostics channels
    /// for `count` log tasks.
    pub fn for_log_tasks(mode: DiagnosticsMode, count: usize) -> Self {
        let channel_names: Vec<ChannelName> = (0..count)
            .map(crate::log_task::log_task_diagnostics_channel)
            .collect();
        LogDiagnosticsTask::new(channel_names, mode)
    }

    pub fn new(channel_names: Vec<ChannelName>, mode: DiagnosticsMode) -> Self {
        let subscribers = channel_names
            .iter()
            .map(|cn| {
                Subscriber::<LogError>::new(SubscriberConfig {
                    is_optional: true,
                    capacity: 1,
                    is_trigger: true,
                    keep_across_runs: true,
                    channel_name: cn.clone(),
                })
            })
            .collect();
        LogDiagnosticsTask {
            channel_names,
            subscribers,
            error_count: 0,
            mode,
        }
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
    fn run(&mut self, _ctx: &Context) {
        let mut subscribers = std::mem::take(&mut self.subscribers);
        for subscriber in subscribers.iter_mut() {
            let mut input = OptionalInput::<LogError>::new(subscriber);
            if let Some(err) = input.value() {
                self.react(err);
                input.clear();
            }
        }
        self.subscribers = subscribers;
    }

    fn for_each_pub_or_sub<'a>(&'a self, f: &mut dyn FnMut(PubOrSub<'a>)) {
        for s in &self.subscribers {
            f(PubOrSub::Subscriber(s));
        }
    }
    fn for_each_pub_or_sub_mut<'a>(&'a mut self, f: &mut dyn FnMut(PubOrSubMut<'a>)) {
        for s in self.subscribers.iter_mut() {
            f(PubOrSubMut::Subscriber(s));
        }
    }
}
