//! Scoped periodic and explicitly event-triggered logging, independent of task macros.
//!
//! Declare captures and flush-event subscribers before allocating storage. Build
//! the workload graph, attach its ExecutionRecorder if needed, then create a
//! LoggingScope and append periodic or event logger callbacks. Release the graph
//! or executor before calling LoggingScope::finish. Keeping a LoggingStatus clone
//! also makes final best-effort cleanup errors observable after scope destruction.
//!
//! Logger callbacks run in an ordinary pool with zero simulated occupancy.
//! Periodic mode keeps a timer active until execution stops; it does not imply EOF.
use crate::{BoxedLogError, Capture, LogFileWriter, LogSession, LogStatus, SharedLogFileWriter};
use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use task::{
    BuiltGraph, Callback, CallbackSchedule, ChannelPlan, Context, EndpointBindings, LoanError,
    ScheduledCallback, Subscriber, SubscriberKey, SubscriberPolicy,
};

#[derive(Clone)]
pub struct LoggingStatus {
    shards: Vec<LogStatus>,
}
impl LoggingStatus {
    pub fn intern_tables(&self) -> crate::InternTables {
        self.shards[0].intern_tables()
    }
    pub fn diagnostics(&self) -> Vec<(usize, crate::LogDiagnostic)> {
        self.shards
            .iter()
            .enumerate()
            .flat_map(|(index, status)| {
                status
                    .diagnostics()
                    .into_iter()
                    .map(move |diagnostic| (index, diagnostic))
            })
            .collect()
    }
    pub fn errors(&self) -> Vec<String> {
        self.shards
            .iter()
            .enumerate()
            .flat_map(|(index, status)| {
                status
                    .errors()
                    .into_iter()
                    .map(move |error| format!("logger shard {index}: {error}"))
            })
            .collect()
    }
}

/// Declare one native flush-event subscriber per logger shard before allocation.
/// Pulses may coalesce; one invocation drains every pending captured message.
pub struct FlushEventPlan {
    keys: Vec<SubscriberKey<()>>,
}
impl FlushEventPlan {
    pub fn declare(plan: &mut ChannelPlan<()>, shards: usize) -> Self {
        Self {
            keys: (0..shards)
                .map(|_| {
                    plan.subscriber_with_policy(
                        1,
                        SubscriberPolicy {
                            trigger: true,
                            keep_across_runs: false,
                        },
                    )
                })
                .collect(),
        }
    }
    pub fn bind<'a>(
        self,
        bindings: &EndpointBindings<'a, ()>,
    ) -> Result<Vec<FlushTrigger<'a>>, task::EndpointError> {
        self.keys
            .into_iter()
            .map(|key| Ok(FlushTrigger::Native(bindings.take_subscriber(&key)?)))
            .collect()
    }
}

pub enum FlushTrigger<'a> {
    Native(Subscriber<'a, ()>),
    #[cfg(feature = "iceoryx2")]
    Ipc(Box<task::iox2::Iox2EventSubscriber>),
}
impl<'a> FlushTrigger<'a> {
    pub fn native(subscriber: Subscriber<'a, ()>) -> Self {
        Self::Native(subscriber)
    }
    #[cfg(feature = "iceoryx2")]
    pub fn ipc(subscriber: task::iox2::Iox2EventSubscriber) -> Self {
        Self::Ipc(Box::new(subscriber))
    }
    fn channel(&self) -> &str {
        match self {
            Self::Native(subscriber) => subscriber.channel_name(),
            #[cfg(feature = "iceoryx2")]
            Self::Ipc(subscriber) => subscriber.channel_name(),
        }
    }
    fn pending(&self) -> bool {
        match self {
            Self::Native(subscriber) => subscriber.has_pending(),
            #[cfg(feature = "iceoryx2")]
            Self::Ipc(subscriber) => subscriber.has_pending(),
        }
    }
    fn ready(&self) -> bool {
        match self {
            Self::Native(subscriber) => !subscriber.is_empty(),
            #[cfg(feature = "iceoryx2")]
            Self::Ipc(subscriber) => {
                let mut ready = false;
                subscriber.visit_records(|record| ready |= record.count != 0);
                ready
            }
        }
    }
    fn update(&self) {
        match self {
            Self::Native(subscriber) => subscriber.update(),
            #[cfg(feature = "iceoryx2")]
            Self::Ipc(subscriber) => subscriber.update(),
        }
    }
    fn clear(&self) {
        match self {
            Self::Native(subscriber) => subscriber.clear(),
            #[cfg(feature = "iceoryx2")]
            Self::Ipc(subscriber) => subscriber.clear(),
        }
    }
    fn set_waker(&mut self, wake: task::wake::WakeHandle) {
        match self {
            Self::Native(subscriber) => subscriber.set_waker(wake),
            #[cfg(feature = "iceoryx2")]
            Self::Ipc(subscriber) => subscriber.set_waker(wake),
        }
    }
}

/// Own sessions outside the executor so final draining happens after callbacks stop.
/// Graphs attached here borrow the scope and must be released before finish/Drop.
///
/// ```compile_fail
/// use logging::{LogFileWriter, LoggingScope};
/// use task::GraphBuilder;
/// use std::time::Duration;
/// fn finish_too_early(writer: impl LogFileWriter + 'static) {
///     let logger = LoggingScope::new(writer, vec![], 1);
///     let graph = logger.attach_periodic(
///         GraphBuilder::new().build().unwrap(), Duration::from_secs(1),
///     ).unwrap();
///     logger.finish().unwrap();
///     drop(graph); // The graph still borrows logger.
/// }
/// ```
pub struct LoggingScope<'storage> {
    registry: crate::intern_tables::Registry,
    sessions: Vec<Mutex<Option<LogSession<'storage>>>>,
    status: LoggingStatus,
    channels: Vec<String>,
    pool: usize,
    attached: AtomicBool,
}
impl<'storage> LoggingScope<'storage> {
    /// Compute before allocation when reserving FlushEventPlan subscribers.
    pub fn planned_shard_count(captured_channels: usize, requested_shards: usize) -> usize {
        requested_shards.max(1).min(captured_channels.max(1))
    }
    /// Round-robin channels across shards; clamp to at least one and at most the
    /// number of captured channels (one empty shard can still drain a recorder).
    pub fn new(
        writer: impl LogFileWriter + 'static,
        captures: Vec<Capture<'storage>>,
        shards: usize,
    ) -> Self {
        let count = Self::planned_shard_count(captures.len(), shards);
        let channels: Vec<String> = captures
            .iter()
            .map(|capture| capture.channel().to_owned())
            .collect();
        let mut groups: Vec<Vec<_>> = (0..count).map(|_| Vec::new()).collect();
        for (index, capture) in captures.into_iter().enumerate() {
            groups[index % count].push(capture);
        }
        let writer = SharedLogFileWriter::new(Box::new(writer));
        let registry = crate::intern_tables::Registry::new(channels.iter().map(String::as_str));
        let sessions: Vec<_> = groups
            .into_iter()
            .map(|captures| LogSession::with_registry(writer.clone(), captures, registry.clone()))
            .collect();
        let status = LoggingStatus {
            shards: sessions.iter().map(LogSession::status).collect(),
        };
        Self {
            registry,
            sessions: sessions
                .into_iter()
                .map(|session| Mutex::new(Some(session)))
                .collect(),
            status,
            channels,
            pool: 0,
            attached: AtomicBool::new(false),
        }
    }
    pub fn shard_count(&self) -> usize {
        self.sessions.len()
    }
    pub fn channels(&self) -> &[String] {
        &self.channels
    }
    pub fn status(&self) -> LoggingStatus {
        self.status.clone()
    }
    pub fn in_pool(mut self, pool: usize) -> Self {
        self.pool = pool;
        self
    }
    pub fn with_diagnostic_policy(mut self, policy: crate::DiagnosticPolicy) -> Self {
        for session in &mut self.sessions {
            session
                .get_mut()
                .unwrap_or_else(|p| p.into_inner())
                .as_mut()
                .unwrap()
                .set_diagnostic_policy(policy);
        }
        self
    }
    /// Attach the recorder to the workload graph first, then configure this scope,
    /// then append logger callbacks. Only shard zero drains recording metadata.
    #[cfg(feature = "serde")]
    pub fn with_recording(
        mut self,
        recorder: task::recording::ExecutionRecorder,
    ) -> Result<Self, BoxedLogError> {
        self.sessions[0]
            .get_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .attach_recording(recorder, self.channels.clone())?;
        Ok(self)
    }
    pub fn attach_periodic<'scope>(
        &'scope self,
        graph: BuiltGraph<'scope>,
        period: Duration,
    ) -> Result<BuiltGraph<'scope>, BoxedLogError>
    where
        'storage: 'scope,
    {
        if period.is_zero() {
            return Err("logging period must be nonzero".into());
        }
        self.attach(
            graph,
            CallbackSchedule::periodic(period),
            (0..self.shard_count()).map(|_| None).collect(),
        )
    }
    /// Explicit events, not captured data arrivals, request event-mode flushes.
    pub fn attach_event<'scope>(
        &'scope self,
        graph: BuiltGraph<'scope>,
        triggers: Vec<FlushTrigger<'storage>>,
    ) -> Result<BuiltGraph<'scope>, BoxedLogError>
    where
        'storage: 'scope,
    {
        if triggers.len() != self.shard_count() {
            return Err("one flush trigger is required per logger shard".into());
        }
        self.attach(
            graph,
            CallbackSchedule::default(),
            triggers.into_iter().map(Some).collect(),
        )
    }
    fn attach<'scope>(
        &'scope self,
        graph: BuiltGraph<'scope>,
        schedule: CallbackSchedule,
        triggers: Vec<Option<FlushTrigger<'storage>>>,
    ) -> Result<BuiltGraph<'scope>, BoxedLogError>
    where
        'storage: 'scope,
    {
        if self.attached.swap(true, Ordering::AcqRel) {
            return Err("logging scope is already attached".into());
        }
        let callbacks =
            self.sessions
                .iter()
                .zip(triggers)
                .enumerate()
                .map(|(index, (session, trigger))| ScheduledCallback {
                    name: format!("LogTask[{index}]"),
                    schedule: schedule
                        .clone()
                        .with_execution_duration(Duration::ZERO)
                        .in_pool(self.pool),
                    callback: Box::new(LogTask { session, trigger }),
                });
        let result = graph
            .append_callbacks(callbacks)
            .map_err(|error| -> BoxedLogError { format!("logging attachment: {error:?}").into() })
            .and_then(|graph| {
                self.registry
                    .configure(crate::InternTables::from_metadata(graph.metadata()))?;
                self.sessions[0]
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_mut()
                    .unwrap()
                    .start()?;
                Ok(graph)
            });
        if result.is_err() {
            self.attached.store(false, Ordering::Release);
        }
        result
    }
    /// Finish every shard even if an earlier shard fails. Scope Drop performs
    /// best-effort LogSession cleanup when explicit finish is not used.
    pub fn finish(mut self) -> Result<(), BoxedLogError> {
        let mut first_error = None;
        for slot in &mut self.sessions {
            if let Some(session) = slot.get_mut().unwrap_or_else(|p| p.into_inner()).take()
                && let Err(error) = session.finish()
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

struct LogTask<'scope, 'storage> {
    session: &'scope Mutex<Option<LogSession<'storage>>>,
    trigger: Option<FlushTrigger<'storage>>,
}
impl Callback for LogTask<'_, '_> {
    fn set_waker(&mut self, wake: task::wake::WakeHandle) {
        if let Some(trigger) = &mut self.trigger {
            trigger.set_waker(wake);
        }
    }
    fn has_pending_inputs(&self) -> bool {
        self.trigger.as_ref().is_some_and(FlushTrigger::pending)
    }
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        if let Some(trigger) = &self.trigger {
            visit(trigger.channel());
        }
    }
    fn update_inputs(&mut self) {
        if let Some(trigger) = &self.trigger {
            trigger.update();
        }
    }
    fn required_inputs_available(&self) -> bool {
        self.trigger.as_ref().is_none_or(FlushTrigger::pending)
    }
    fn required_inputs_ready(&self) -> bool {
        self.trigger.as_ref().is_none_or(FlushTrigger::ready)
    }
    fn run(&mut self, context: &Context) -> Result<(), LoanError> {
        if let Some(trigger) = &self.trigger {
            trigger.clear();
        }
        let mut session = self
            .session
            .lock()
            .map_err(|_| LoanError::Transport("logging session poisoned".into()))?;
        // LogSession records and persists failures. Error policies are separate
        // from scheduling; a recoverable sink failure does not stop the workload.
        let _ = session
            .as_mut()
            .expect("logging session must outlive its graph")
            .flush_at(context.now);
        Ok(())
    }
    #[cfg(feature = "iceoryx2")]
    fn take_iox2_events(&mut self) -> Vec<task::iox2::Iox2EventRegistration> {
        match &mut self.trigger {
            Some(FlushTrigger::Ipc(subscriber)) => {
                subscriber.take_registration().into_iter().collect()
            }
            _ => Vec::new(),
        }
    }
}
