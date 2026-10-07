use crate::string_interner::{CallbackNameInterner, ChannelNameInterner};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use super::{Callback, Context, LoanError, execute_callback};
use crate::time::FrameworkTime;

pub type FactoryError = Box<dyn std::error::Error + Send + Sync>;
type StoredCallback<'storage> = Box<dyn Callback + 'storage>;
type Factory<'build, 'storage> =
    Box<dyn FnOnce() -> Result<StoredCallback<'storage>, FactoryError> + 'build>;

#[derive(Debug)]
pub enum GraphBuildError {
    DuplicateCallback(String),
    ZeroPeriod(String),
    Factory {
        callback: String,
        source: FactoryError,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct GraphStepError {
    pub callback: String,
    pub source: LoanError,
}

/// Deferred callback construction after storage and typed bindings exist.
/// Factories can borrow temporary bindings for `'build`; returned callbacks only
/// retain their storage borrows for `'storage` and can move to scoped workers.
pub struct GraphBuilder<'build, 'storage> {
    factories: Vec<(String, CallbackSchedule, Factory<'build, 'storage>)>,
    channel_names: Arc<ChannelNameInterner>,
}

impl<'build, 'storage> Default for GraphBuilder<'build, 'storage> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'build, 'storage> GraphBuilder<'build, 'storage> {
    pub fn new() -> Self {
        Self {
            factories: Vec::new(),
            channel_names: Arc::new(ChannelNameInterner::new()),
        }
    }

    /// Retain the finalized channel interner, including fixture-only channels.
    pub fn with_storage<S>(storage: &crate::GraphStorage<S>) -> Self {
        Self {
            factories: Vec::new(),
            channel_names: storage.channel_names().clone(),
        }
    }

    pub fn add_callback<F, C>(&mut self, name: impl Into<String>, factory: F)
    where
        F: FnOnce() -> Result<C, FactoryError> + 'build,
        C: Callback + 'storage,
    {
        self.add_scheduled_callback(name, CallbackSchedule::default(), factory);
    }

    pub fn add_scheduled_callback<F, C>(
        &mut self,
        name: impl Into<String>,
        schedule: CallbackSchedule,
        factory: F,
    ) where
        F: FnOnce() -> Result<C, FactoryError> + 'build,
        C: Callback + 'storage,
    {
        self.factories.push((
            name.into(),
            schedule,
            Box::new(move || {
                factory().map(|callback| Box::new(callback) as StoredCallback<'storage>)
            }),
        ));
    }

    /// Validate names before running factories. Failure drops all successfully
    /// constructed callbacks and the unexecuted factories by normal destruction.
    pub fn build(mut self) -> Result<BuiltGraph<'storage>, GraphBuildError> {
        let mut names = HashSet::new();
        for (name, schedule, _) in &self.factories {
            if schedule.period.is_some_and(|period| period.is_zero()) {
                return Err(GraphBuildError::ZeroPeriod(name.clone()));
            }
            if !names.insert(name) {
                return Err(GraphBuildError::DuplicateCallback(name.clone()));
            }
        }
        let mut callbacks = Vec::with_capacity(self.factories.len());
        let mut callback_names = CallbackNameInterner::new();
        for (name, schedule, factory) in self.factories {
            let callback = factory().map_err(|source| GraphBuildError::Factory {
                callback: name.clone(),
                source,
            })?;
            callback_names.intern(&name);
            callback.visit_channel_names(&mut |channel| {
                if !channel.is_empty() && self.channel_names.lookup_by_value(channel).is_none() {
                    Arc::make_mut(&mut self.channel_names).intern(channel);
                }
            });
            callbacks.push(ScheduledCallback {
                name,
                callback,
                schedule,
            });
        }
        callback_names.shrink_to_fit();
        Ok(BuiltGraph {
            callbacks,
            metadata: GraphMetadata {
                channel_names: self.channel_names,
                callback_names: Arc::new(callback_names),
            },
        })
    }
}

/// Borrowed callback graph with an explicit insertion-order stepping operation.
/// This is a construction/lifecycle harness, not the readiness or timing scheduler.
/// Callbacks own typed endpoints; the executor invocation manages their IO lifecycle.
pub struct BuiltGraph<'storage> {
    callbacks: Vec<ScheduledCallback<'storage>>,
    metadata: GraphMetadata,
}

impl<'storage> BuiltGraph<'storage> {
    pub fn metadata(&self) -> &GraphMetadata {
        &self.metadata
    }
    pub fn into_parts(self) -> (Vec<ScheduledCallback<'storage>>, GraphMetadata) {
        (self.callbacks, self.metadata)
    }
    /// Stop at the first error. Earlier callbacks may already have published;
    /// the step is not transactional and later callbacks are not executed.
    pub fn step(&mut self, timestamp: FrameworkTime) -> Result<(), GraphStepError> {
        for node in &mut self.callbacks {
            execute_callback(node.callback.as_mut(), &self.metadata.context(timestamp)).map_err(
                |source| GraphStepError {
                    callback: node.name.clone(),
                    source,
                },
            )?;
        }
        Ok(())
    }
}

// TODO(port-timing): Add modeled execution duration and custom next-run timing for
// simulated busy-until times and virtual-pool occupancy, with explicit timestamp rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct CallbackSchedule {
    pub pool: usize,
    // TODO(port-startup-policy): Honor this flag consistently in live and simulation;
    // test explicit startup execution independently of the first periodic deadline.
    pub run_on_start: bool,
    pub period: Option<Duration>,
}

impl CallbackSchedule {
    pub fn on_start() -> Self {
        Self {
            run_on_start: true,
            ..Self::default()
        }
    }
    /// First periodic execution is requested after one period.
    pub fn periodic(period: Duration) -> Self {
        Self {
            period: Some(period),
            ..Self::default()
        }
    }
    pub fn in_pool(mut self, pool: usize) -> Self {
        self.pool = pool;
        self
    }
}

pub struct ScheduledCallback<'storage> {
    pub name: String,
    pub callback: Box<dyn Callback + 'storage>,
    pub schedule: CallbackSchedule,
}

/// Frozen name tables shared by every invocation and executor thread.
#[derive(Clone)]
pub struct GraphMetadata {
    pub channel_names: Arc<ChannelNameInterner>,
    pub callback_names: Arc<CallbackNameInterner>,
}

impl GraphMetadata {
    pub fn context(&self, now: FrameworkTime) -> Context<'_> {
        Context::new(now, &self.channel_names, &self.callback_names)
    }
}
