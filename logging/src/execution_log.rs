//! Declares and binds the execution-log channels before allocation.
use crate::Capture;
use std::collections::BTreeMap;
use task::{
    ChannelPlan, ChannelStorage, PublisherKey, SubscriberKey,
    execution_log_schema::{
        EXECUTION_LOG_CHANNEL, ExecutionLogMode, ExecutionLogModes, ExecutionRecord,
        IOX2_EVENT_RECORD_CHANNEL, Iox2EventRecord,
    },
};
use task_graph::{
    BuildError, BuiltGraph,
    execution_log::{ExecutionLogMetadata, ExecutionLogPublishers},
};

/// Configuration for execution logging.
///
/// Capacities are capture subscriber depths. Publisher loan capacity is one
/// message per publisher, so the channel arena is derived by the normal pub/sub
/// planner.
#[derive(Clone, Debug)]
pub struct ExecutionLogOptions {
    pub modes: ExecutionLogModes,
    pub execution_log_capacity: usize,
    pub iox2_event_log_capacity: usize,
}
impl ExecutionLogOptions {
    pub fn new(execution_log_capacity: usize, iox2_event_log_capacity: usize) -> Self {
        Self {
            modes: ExecutionLogModes::default(),
            execution_log_capacity,
            iox2_event_log_capacity,
        }
    }
    pub fn with_modes(mut self, modes: ExecutionLogModes) -> Self {
        self.modes = modes;
        self
    }
    fn validate(&self) -> Result<(), BuildError> {
        if self.execution_log_capacity == 0 {
            return Err(BuildError("execution log capacity must be nonzero".into()));
        }
        if self.iox2_event_log_capacity == 0 {
            return Err(BuildError("iox2 event log capacity must be nonzero".into()));
        }
        Ok(())
    }
}

/// Pre-allocation declaration of execution-log publishers and captures.
///
/// Execution-log channels are independent of workload storage. Build the graph
/// first, then create this plan from its callback metadata.
pub struct ExecutionLogPlan {
    modes: ExecutionLogModes,
    record_plan: ChannelPlan<ExecutionRecord>,
    event_plan: ChannelPlan<Iox2EventRecord>,
    record_publishers: BTreeMap<String, PublisherKey<ExecutionRecord>>,
    event_publishers: BTreeMap<String, PublisherKey<Iox2EventRecord>>,
    record_capture: SubscriberKey<ExecutionRecord>,
    event_capture: SubscriberKey<Iox2EventRecord>,
}

impl ExecutionLogPlan {
    /// Build a plan from the graph's callback metadata.
    pub fn for_graph(
        graph: &BuiltGraph<'_>,
        options: ExecutionLogOptions,
    ) -> Result<Self, BuildError> {
        options.validate()?;
        for name in options.modes.callbacks.keys() {
            if graph
                .metadata()
                .callback_names
                .lookup_by_value(name)
                .is_none()
            {
                return Err(BuildError(format!(
                    "unknown execution log callback '{name}'"
                )));
            }
        }

        let mut record_plan = ChannelPlan::<ExecutionRecord>::new(EXECUTION_LOG_CHANNEL);
        let mut event_plan = ChannelPlan::<Iox2EventRecord>::new(IOX2_EVENT_RECORD_CHANNEL);
        let mut record_publishers = BTreeMap::new();
        let mut event_publishers = BTreeMap::new();
        for name in graph.metadata().callback_names.names() {
            if options.modes.mode_for(name) == ExecutionLogMode::Off {
                continue;
            }
            record_publishers.insert(name.to_owned(), record_plan.publisher(1));
            event_publishers.insert(name.to_owned(), event_plan.publisher(1));
        }
        let policy = task::SubscriberPolicy {
            trigger: false,
            keep_across_runs: true,
        };
        let record_capture =
            record_plan.subscriber_with_policy(options.execution_log_capacity, policy);
        let event_capture =
            event_plan.subscriber_with_policy(options.iox2_event_log_capacity, policy);
        Ok(Self {
            modes: options.modes,
            record_plan,
            event_plan,
            record_publishers,
            event_publishers,
            record_capture,
            event_capture,
        })
    }

    pub fn allocate(self) -> Result<ExecutionLogStorage, BuildError> {
        Ok(ExecutionLogStorage {
            records: self.record_plan.allocate()?,
            events: self.event_plan.allocate()?,
            modes: self.modes,
            record_publishers: self.record_publishers,
            event_publishers: self.event_publishers,
            record_capture: self.record_capture,
            event_capture: self.event_capture,
        })
    }
}

/// Allocated execution-log storage plus its publisher and capture keys.
///
/// Keep this alive while the attached graph and log session borrow it.
pub struct ExecutionLogStorage {
    records: ChannelStorage<ExecutionRecord>,
    events: ChannelStorage<Iox2EventRecord>,
    modes: ExecutionLogModes,
    record_publishers: BTreeMap<String, PublisherKey<ExecutionRecord>>,
    event_publishers: BTreeMap<String, PublisherKey<Iox2EventRecord>>,
    record_capture: SubscriberKey<ExecutionRecord>,
    event_capture: SubscriberKey<Iox2EventRecord>,
}

impl ExecutionLogStorage {
    pub fn attach<'a>(
        &'a self,
        graph: BuiltGraph<'a>,
    ) -> Result<(BuiltGraph<'a>, ExecutionLogMetadata, Vec<Capture<'a>>), BuildError> {
        let records = self.records.bind();
        let events = self.events.bind();
        let mut record_publishers = BTreeMap::new();
        for (name, key) in &self.record_publishers {
            record_publishers.insert(name.clone(), records.take_publisher(key)?);
        }
        let mut event_publishers = BTreeMap::new();
        for (name, key) in &self.event_publishers {
            event_publishers.insert(name.clone(), events.take_publisher(key)?);
        }
        let record_capture = Capture::native(records.take_subscriber(&self.record_capture)?);
        let event_capture = Capture::native(events.take_subscriber(&self.event_capture)?);
        let (graph, metadata) = graph
            .with_execution_log(
                self.modes.clone(),
                ExecutionLogPublishers {
                    records: record_publishers,
                    iox2_events: event_publishers,
                },
            )
            .map_err(BuildError)?;
        Ok((graph, metadata, vec![record_capture, event_capture]))
    }
}
