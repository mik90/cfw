//! Declares and binds the execution-log channel before graph allocation.
use crate::Capture;
use std::collections::BTreeMap;
use task::{
    PublisherKey, SubscriberKey,
    execution_log_schema::{
        EXECUTION_LOG_CHANNEL, ExecutionRecord, IOX2_EVENT_RECORD_CHANNEL, Iox2EventRecord,
        RecordingOptions,
    },
};
use task_graph::{
    BuildError, BuiltGraph, GraphBindings, GraphPlan,
    execution_log::{ExecutionLogMetadata, ExecutionLogPublishers},
};

/// Configuration for execution logging.
///
/// Capacities are capture subscriber depths. Publisher loan capacity is one
/// message per publisher, so the channel arena is derived by the normal pub/sub
/// planner.
#[derive(Clone, Debug)]
pub struct ExecutionLogOptions {
    pub recording: RecordingOptions,
    pub execution_log_capacity: usize,
    pub iox2_event_log_capacity: usize,
}
impl ExecutionLogOptions {
    pub fn new(
        recording: RecordingOptions,
        execution_log_capacity: usize,
        iox2_event_log_capacity: usize,
    ) -> Self {
        Self {
            recording,
            execution_log_capacity,
            iox2_event_log_capacity,
        }
    }
    pub fn with_default_recording(
        execution_log_capacity: usize,
        iox2_event_log_capacity: usize,
    ) -> Self {
        Self::new(
            RecordingOptions::default(),
            execution_log_capacity,
            iox2_event_log_capacity,
        )
    }
}

/// Pre-allocation declaration of execution-log publishers and captures.
pub struct ExecutionLogPlan {
    recording: RecordingOptions,
    record_publishers: BTreeMap<String, PublisherKey<ExecutionRecord>>,
    event_publishers: BTreeMap<String, PublisherKey<Iox2EventRecord>>,
    record_capture: SubscriberKey<ExecutionRecord>,
    event_capture: SubscriberKey<Iox2EventRecord>,
}

impl ExecutionLogPlan {
    /// Declare one publisher per recorded callback and one capture per log
    /// stream. Call after registering tasks and before allocation.
    pub fn declare(plan: &mut GraphPlan, options: ExecutionLogOptions) -> Result<Self, BuildError> {
        if options.execution_log_capacity == 0 {
            return Err(BuildError("execution log capacity must be nonzero".into()));
        }
        if options.iox2_event_log_capacity == 0 {
            return Err(BuildError("iox2 event log capacity must be nonzero".into()));
        }
        let mut record_publishers = BTreeMap::new();
        let mut event_publishers = BTreeMap::new();
        for name in plan.task_names().map(str::to_owned).collect::<Vec<_>>() {
            if options.recording.mode_for(&name) == task::execution_log_schema::RecordingMode::Off {
                continue;
            }
            record_publishers.insert(
                name.clone(),
                plan.native::<ExecutionRecord>(EXECUTION_LOG_CHANNEL)?
                    .publisher(1),
            );
            event_publishers.insert(
                name,
                plan.native::<Iox2EventRecord>(IOX2_EVENT_RECORD_CHANNEL)?
                    .publisher(1),
            );
        }
        let policy = task::SubscriberPolicy {
            trigger: false,
            keep_across_runs: true,
        };
        let record_capture = plan
            .native::<ExecutionRecord>(EXECUTION_LOG_CHANNEL)?
            .subscriber_with_policy(options.execution_log_capacity, policy);
        let event_capture = plan
            .native::<Iox2EventRecord>(IOX2_EVENT_RECORD_CHANNEL)?
            .subscriber_with_policy(options.iox2_event_log_capacity, policy);
        Ok(Self {
            recording: options.recording,
            record_publishers,
            event_publishers,
            record_capture,
            event_capture,
        })
    }

    /// Bind the publishers and capture. Call after `GraphStorage::bind`.
    pub fn bind<'a>(
        self,
        bindings: &GraphBindings<'a>,
    ) -> Result<ExecutionLogBindings<'a>, BuildError> {
        let records = bindings.native::<ExecutionRecord>(EXECUTION_LOG_CHANNEL)?;
        let events = bindings.native::<Iox2EventRecord>(IOX2_EVENT_RECORD_CHANNEL)?;
        let mut record_publishers = BTreeMap::new();
        for (name, key) in self.record_publishers {
            record_publishers.insert(name, records.take_publisher(&key)?);
        }
        let mut event_publishers = BTreeMap::new();
        for (name, key) in self.event_publishers {
            event_publishers.insert(name, events.take_publisher(&key)?);
        }
        let record_capture = records.take_subscriber(&self.record_capture)?;
        let event_capture = events.take_subscriber(&self.event_capture)?;
        Ok(ExecutionLogBindings {
            options: self.recording,
            publishers: ExecutionLogPublishers {
                records: record_publishers,
                iox2_events: event_publishers,
            },
            captures: vec![
                Capture::native(record_capture),
                Capture::native(event_capture),
            ],
        })
    }

    /// Bind and attach in one step.
    pub fn attach<'a>(
        self,
        graph: BuiltGraph<'a>,
        bindings: &GraphBindings<'a>,
    ) -> Result<(BuiltGraph<'a>, ExecutionLogMetadata, Vec<Capture<'a>>), BuildError> {
        self.bind(bindings)?.attach(graph)
    }
}

/// Bound execution-log publishers and captures.
pub struct ExecutionLogBindings<'a> {
    options: RecordingOptions,
    publishers: ExecutionLogPublishers<'a>,
    captures: Vec<Capture<'a>>,
}

impl<'a> ExecutionLogBindings<'a> {
    /// Attach publishers to callbacks and return the log captures plus metadata.
    pub fn attach(
        self,
        graph: BuiltGraph<'a>,
    ) -> Result<(BuiltGraph<'a>, ExecutionLogMetadata, Vec<Capture<'a>>), BuildError> {
        let (graph, metadata) = graph
            .with_execution_log(self.options, self.publishers)
            .map_err(BuildError)?;
        Ok((graph, metadata, self.captures))
    }
}

/// Standalone execution-log plan for graphs built without a `GraphPlan`.
///
/// The workload graph and the execution-log channel allocate separately. Keep
/// the resulting storage alive while the graph and log session borrow it.
pub struct StandaloneExecutionLogPlan {
    recording: RecordingOptions,
    publishers: BTreeMap<String, task::PublisherKey<ExecutionRecord>>,
    event_publishers: BTreeMap<String, task::PublisherKey<Iox2EventRecord>>,
    capture: task::SubscriberKey<ExecutionRecord>,
    event_capture: task::SubscriberKey<Iox2EventRecord>,
    plan: task::ChannelPlan<ExecutionRecord>,
    event_plan: task::ChannelPlan<Iox2EventRecord>,
}

impl StandaloneExecutionLogPlan {
    pub fn for_graph(
        graph: &BuiltGraph<'_>,
        options: ExecutionLogOptions,
    ) -> Result<Self, BuildError> {
        if options.execution_log_capacity == 0 {
            return Err(BuildError("execution log capacity must be nonzero".into()));
        }
        if options.iox2_event_log_capacity == 0 {
            return Err(BuildError("iox2 event log capacity must be nonzero".into()));
        }
        let mut plan = task::ChannelPlan::<ExecutionRecord>::new(EXECUTION_LOG_CHANNEL);
        let mut event_plan = task::ChannelPlan::<Iox2EventRecord>::new(IOX2_EVENT_RECORD_CHANNEL);
        let mut publishers = BTreeMap::new();
        let mut event_publishers = BTreeMap::new();
        for name in graph.metadata().callback_names.names() {
            if options.recording.mode_for(name) == task::execution_log_schema::RecordingMode::Off {
                continue;
            }
            publishers.insert(name.to_owned(), plan.publisher(1));
            event_publishers.insert(name.to_owned(), event_plan.publisher(1));
        }
        let policy = task::SubscriberPolicy {
            trigger: false,
            keep_across_runs: true,
        };
        let capture = plan.subscriber_with_policy(options.execution_log_capacity, policy);
        let event_capture =
            event_plan.subscriber_with_policy(options.iox2_event_log_capacity, policy);
        Ok(Self {
            recording: options.recording,
            publishers,
            event_publishers,
            capture,
            event_capture,
            plan,
            event_plan,
        })
    }

    pub fn allocate(self) -> Result<StandaloneExecutionLog, BuildError> {
        Ok(StandaloneExecutionLog {
            storage: self.plan.allocate()?,
            event_storage: self.event_plan.allocate()?,
            options: self.recording,
            publishers: self.publishers,
            event_publishers: self.event_publishers,
            capture: self.capture,
            event_capture: self.event_capture,
        })
    }
}

/// Allocated execution-log storage plus its publisher and capture keys.
pub struct StandaloneExecutionLog {
    storage: task::ChannelStorage<ExecutionRecord>,
    event_storage: task::ChannelStorage<Iox2EventRecord>,
    options: RecordingOptions,
    publishers: BTreeMap<String, task::PublisherKey<ExecutionRecord>>,
    event_publishers: BTreeMap<String, task::PublisherKey<Iox2EventRecord>>,
    capture: task::SubscriberKey<ExecutionRecord>,
    event_capture: task::SubscriberKey<Iox2EventRecord>,
}

impl StandaloneExecutionLog {
    pub fn attach<'a>(
        &'a self,
        graph: BuiltGraph<'a>,
    ) -> Result<(BuiltGraph<'a>, ExecutionLogMetadata, Vec<Capture<'a>>), BuildError> {
        let bindings = self.storage.bind();
        let event_bindings = self.event_storage.bind();
        let mut publishers = BTreeMap::new();
        for (name, key) in &self.publishers {
            publishers.insert(name.clone(), bindings.take_publisher(key)?);
        }
        let mut event_publishers = BTreeMap::new();
        for (name, key) in &self.event_publishers {
            event_publishers.insert(name.clone(), event_bindings.take_publisher(key)?);
        }
        let capture = Capture::native(bindings.take_subscriber(&self.capture)?);
        let event_capture = Capture::native(event_bindings.take_subscriber(&self.event_capture)?);
        let (graph, metadata) = graph
            .with_execution_log(
                self.options.clone(),
                ExecutionLogPublishers {
                    records: publishers,
                    iox2_events: event_publishers,
                },
            )
            .map_err(BuildError)?;
        Ok((graph, metadata, vec![capture, event_capture]))
    }
}
