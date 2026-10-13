use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use task::string_interner::{CallbackNameInterner, ChannelNameInterner};

use task::time::FrameworkTime;
use task::{Callback, Context};

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

    /// Include channels without callback endpoints, such as fixture-only channels.
    /// Names are sorted to keep their IDs independent of declaration order.
    pub fn with_channel_names(names: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut names: Vec<_> = names.into_iter().map(|name| name.as_ref().to_owned()).collect();
        names.sort();
        names.dedup();
        let mut channel_names = ChannelNameInterner::new();
        for name in names { channel_names.intern(&name); }
        channel_names.shrink_to_fit();
        Self {
            factories: Vec::new(),
            channel_names: Arc::new(channel_names),
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
    pub fn build(self) -> Result<BuiltGraph<'storage>, GraphBuildError> {
        validate_callbacks(
            self.factories
                .iter()
                .map(|(name, schedule, _)| (name.as_str(), schedule)),
        )?;
        let mut callbacks = Vec::with_capacity(self.factories.len());
        for (name, schedule, factory) in self.factories {
            let callback = factory().map_err(|source| GraphBuildError::Factory {
                callback: name.clone(),
                source,
            })?;
            callbacks.push(ScheduledCallback {
                name,
                callback,
                schedule,
            });
        }
        Ok(BuiltGraph::from_callbacks(callbacks, self.channel_names))
    }
}

pub(crate) fn validate_callbacks<'a>(
    callbacks: impl IntoIterator<Item = (&'a str, &'a CallbackSchedule)>,
) -> Result<(), GraphBuildError> {
    let mut names = HashSet::new();
    for (name, schedule) in callbacks {
        if schedule.period.is_some_and(|period| period.is_zero()) {
            return Err(GraphBuildError::ZeroPeriod(name.into()));
        }
        if !names.insert(name) {
            return Err(GraphBuildError::DuplicateCallback(name.into()));
        }
    }
    Ok(())
}

/// Callbacks and their metadata, ready to hand to an executor. Typed endpoints
/// retain their storage borrows independently of temporary construction bindings.
pub struct BuiltGraph<'storage> {
    callbacks: Vec<ScheduledCallback<'storage>>,
    metadata: GraphMetadata,
}

impl<'storage> BuiltGraph<'storage> {
    /// Names and schedules must be validated before constructing callbacks.
    pub(crate) fn from_callbacks(
        callbacks: Vec<ScheduledCallback<'storage>>,
        channel_names: Arc<ChannelNameInterner>,
    ) -> Self {
        let mut graph = Self {
            callbacks: Vec::new(),
            metadata: GraphMetadata {
                channel_names,
                callback_names: Arc::new(CallbackNameInterner::new()),
            },
        };
        graph.extend(callbacks);
        Arc::make_mut(&mut graph.metadata.callback_names).shrink_to_fit();
        graph
    }

    fn extend(&mut self, callbacks: Vec<ScheduledCallback<'storage>>) {
        for node in &callbacks {
            Arc::make_mut(&mut self.metadata.callback_names).intern(&node.name);
            node.callback.visit_channel_names(&mut |channel| {
                if !channel.is_empty()
                    && self
                        .metadata
                        .channel_names
                        .lookup_by_value(channel)
                        .is_none()
                {
                    Arc::make_mut(&mut self.metadata.channel_names).intern(channel);
                }
            });
        }
        self.callbacks.extend(callbacks);
    }
    /// Attach full execution recording to this graph.
    pub fn with_recording(
        self,
        recorder: &task::recording::ExecutionRecorder,
    ) -> Result<Self, String> {
        self.with_recording_options(recorder, task::recording::RecordingOptions::default())
    }

    /// Attach recording with per-callback overrides. Unknown names are rejected.
    pub fn with_recording_options(
        self,
        recorder: &task::recording::ExecutionRecorder,
        options: task::recording::RecordingOptions,
    ) -> Result<Self, String> {
        use task::recording::{CallbackDescriptor, RecordingMode};

        for name in options.callbacks.keys() {
            if self.metadata.callback_names.lookup_by_value(name).is_none() {
                return Err(format!("unknown recording callback '{name}'"));
            }
        }
        let mut attachment = recorder.begin_attachment(self.metadata.intern_tables())?;
        let graph = self.try_map_callbacks(|_, name, callback| {
            let mode = options
                .callbacks
                .get(name)
                .copied()
                .unwrap_or(options.default_mode);
            let endpoints = match (mode, callback.recording_endpoints()) {
                (RecordingMode::Full, None) => {
                    return Err(format!(
                        "callback '{name}' does not implement recording metadata"
                    ));
                }
                (_, endpoints) => endpoints.unwrap_or_default(),
            };
            attachment.wrap_callback(
                CallbackDescriptor {
                    name: name.into(),
                    recording_mode: mode,
                    endpoints,
                },
                callback,
            )
        })?;
        attachment.finish();
        Ok(graph)
    }

    /// Append infrastructure callbacks without changing existing callback indices.
    pub fn append_callbacks(
        mut self,
        callbacks: impl IntoIterator<Item = ScheduledCallback<'storage>>,
    ) -> Result<Self, GraphBuildError> {
        let callbacks: Vec<_> = callbacks.into_iter().collect();
        validate_callbacks(
            self.callbacks
                .iter()
                .chain(&callbacks)
                .map(|node| (node.name.as_str(), &node.schedule)),
        )?;
        self.extend(callbacks);
        Ok(self)
    }
    pub fn try_map_callbacks<E>(
        self,
        mut map: impl FnMut(
            usize,
            &str,
            Box<dyn Callback + 'storage>,
        ) -> Result<Box<dyn Callback + 'storage>, E>,
    ) -> Result<Self, E> {
        let callbacks = self
            .callbacks
            .into_iter()
            .enumerate()
            .map(|(index, node)| {
                Ok(ScheduledCallback {
                    callback: map(index, &node.name, node.callback)?,
                    name: node.name,
                    schedule: node.schedule,
                })
            })
            .collect::<Result<_, E>>()?;
        Ok(Self {
            callbacks,
            metadata: self.metadata,
        })
    }
    pub fn metadata(&self) -> &GraphMetadata {
        &self.metadata
    }
    /// Override scheduling policy by callback name before handing off to an executor.
    pub fn configure_schedules(&mut self, mut configure: impl FnMut(&str, &mut CallbackSchedule)) {
        for callback in &mut self.callbacks {
            configure(&callback.name, &mut callback.schedule);
        }
    }
    pub fn into_parts(self) -> (Vec<ScheduledCallback<'storage>>, GraphMetadata) {
        (self.callbacks, self.metadata)
    }
}

#[derive(Clone, Default)]
pub struct CallbackSchedule {
    pub pool: usize,
    pub run_on_start: bool,
    pub period: Option<Duration>,
    /// Explicit simulated pool occupancy. Simulation rejects an unspecified
    /// duration; live execution measures actual completion instead.
    pub execution_duration: Option<Duration>,
    pub execution_duration_callback: Option<Arc<dyn Fn() -> Duration + Send + Sync>>,
    pub next_execution_time_callback:
        Option<Arc<dyn Fn(FrameworkTime) -> Option<FrameworkTime> + Send + Sync>>,
}

impl std::fmt::Debug for CallbackSchedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackSchedule")
            .field("pool", &self.pool)
            .field("run_on_start", &self.run_on_start)
            .field("period", &self.period)
            .field("execution_duration", &self.execution_duration)
            .field(
                "custom_duration",
                &self.execution_duration_callback.is_some(),
            )
            .field(
                "custom_next_time",
                &self.next_execution_time_callback.is_some(),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingError {
    MissingDuration,
    Overflow,
    PastDeadline,
}

impl CallbackSchedule {
    pub fn with_execution_duration(mut self, duration: Duration) -> Self {
        self.execution_duration = Some(duration);
        self.execution_duration_callback = None;
        self
    }
    pub fn with_execution_duration_callback(
        mut self,
        duration: impl Fn() -> Duration + Send + Sync + 'static,
    ) -> Self {
        self.execution_duration_callback = Some(Arc::new(duration));
        self
    }
    pub fn with_next_execution_time_callback(
        mut self,
        next: impl Fn(FrameworkTime) -> Option<FrameworkTime> + Send + Sync + 'static,
    ) -> Self {
        self.next_execution_time_callback = Some(Arc::new(next));
        self.period = None;
        self
    }
    pub fn has_execution_duration(&self) -> bool {
        self.execution_duration.is_some() || self.execution_duration_callback.is_some()
    }
    pub fn duration(&self) -> Result<Duration, TimingError> {
        self.execution_duration_callback
            .as_ref()
            .map(|duration| duration())
            .or(self.execution_duration)
            .ok_or(TimingError::MissingDuration)
    }
    pub fn is_timed(&self) -> bool {
        self.period.is_some() || self.next_execution_time_callback.is_some()
    }
    /// Compute a deadline from completion time, or from start time for the first
    /// invocation. A custom callback returning None disables its timer until an
    /// input-triggered invocation computes another deadline.
    pub fn next_after(&self, now: FrameworkTime) -> Result<Option<FrameworkTime>, TimingError> {
        let next = if let Some(next) = &self.next_execution_time_callback {
            next(now)
        } else if let Some(period) = self.period {
            Some(
                now.checked_add_duration(period)
                    .ok_or(TimingError::Overflow)?,
            )
        } else {
            None
        };
        if next.is_some_and(|next| next < now) {
            return Err(TimingError::PastDeadline);
        }
        Ok(next)
    }
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
    /// Clone the frozen ID space for recording and logging metadata.
    pub fn intern_tables(&self) -> task::recording::InternTables {
        task::recording::InternTables {
            channels: (*self.channel_names).clone(),
            callbacks: (*self.callback_names).clone(),
        }
    }

    pub fn context(&self, now: FrameworkTime) -> Context<'_> {
        Context::new(now, &self.channel_names, &self.callback_names)
    }
}
