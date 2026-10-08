use crate::{FrameworkTime, SimulationConfig};
use std::collections::BTreeMap;
use std::num::Saturating;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use task::wake::Wake;
use task::{
    BatchExecutionError, BatchFailure, BuiltGraph, Callback, CallbackSchedule, GraphMetadata,
    TimingError, execute_callback_batch,
};

const TRIGGER: u8 = 1;
const READINESS: u8 = 2;
struct Notification(AtomicU8);
impl Wake for Notification {
    fn wake(&self) {
        self.0.fetch_or(TRIGGER, Ordering::Release);
    }
    fn readiness_changed(&self) {
        self.0.fetch_or(READINESS, Ordering::Release);
    }
}

#[derive(Debug)]
pub enum StepError {
    InvalidConfig(String),
    Callback {
        callback: String,
        failure: BatchFailure,
    },
    Timing {
        callback: String,
        source: TimingError,
    },
    Panicked,
    Poisoned,
    Action(String),
    PastAction,
    UnknownEventChannel(String),
    Iox2Event(String),
    StepLimitExceeded,
}
impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "simulation error: {self:?}")
    }
}
impl std::error::Error for StepError {}

#[derive(Debug)]
pub struct StepResult {
    pub before: FrameworkTime,
    pub after: FrameworkTime,
    /// Callback indices in deterministic scheduling/commit order.
    pub executed: Vec<usize>,
    /// No runnable work or future simulated event. External inputs may wake it.
    pub idle: bool,
}

struct NodeState {
    requested: bool,
    ready_since: Option<FrameworkTime>,
    busy_until: FrameworkTime,
    next: Option<FrameworkTime>,
}

type Action<'storage> = Box<dyn FnOnce(FrameworkTime) -> Result<(), String> + Send + 'storage>;

/// Borrowed, discrete-event simulation with deterministic batch commits.
/// A failed step poisons the session: callback state and consumed inputs cannot
/// be rolled back. Retained message handles remain valid for their storage borrow.
pub struct SimulationState<'storage> {
    callbacks: Vec<Box<dyn Callback + 'storage>>,
    names: Vec<String>,
    schedules: Vec<CallbackSchedule>,
    nodes: Vec<NodeState>,
    notifications: Vec<Arc<Notification>>,
    metadata: GraphMetadata,
    config: SimulationConfig,
    time: FrameworkTime,
    step_count: Saturating<usize>,
    failed: bool,
    actions: BTreeMap<(FrameworkTime, usize), Action<'storage>>,
    action_sequence: usize,
    #[cfg(feature = "iceoryx2")]
    events: Vec<task::iox2::Iox2EventRegistration>,
}

impl<'storage> SimulationState<'storage> {
    pub fn new(graph: BuiltGraph<'storage>) -> Result<Self, StepError> {
        Self::with_config(graph, SimulationConfig::default())
    }

    pub fn with_config(
        graph: BuiltGraph<'storage>,
        config: SimulationConfig,
    ) -> Result<Self, StepError> {
        if config.node_executor_thread_count == 0
            || config.virtual_pool_threads.is_empty()
            || config.virtual_pool_threads.contains(&0)
            || config.start_time == FrameworkTime::INVALID
        {
            return Err(StepError::InvalidConfig(
                "positive worker/pool counts and a valid start time are required".into(),
            ));
        }
        let (callbacks, metadata) = graph.into_parts();
        let mut state = Self {
            callbacks: Vec::new(),
            names: Vec::new(),
            schedules: Vec::new(),
            nodes: Vec::new(),
            notifications: Vec::new(),
            metadata,
            time: config.start_time,
            config,
            step_count: Saturating(0),
            failed: false,
            actions: BTreeMap::new(),
            action_sequence: 0,
            #[cfg(feature = "iceoryx2")]
            events: Vec::new(),
        };
        for mut node in callbacks {
            if !node.schedule.has_execution_duration() {
                return Err(StepError::Timing {
                    callback: node.name,
                    source: TimingError::MissingDuration,
                });
            }
            if node.schedule.pool >= state.config.virtual_pool_threads.len() {
                return Err(StepError::InvalidConfig(format!(
                    "callback '{}' references missing pool {}",
                    node.name, node.schedule.pool
                )));
            }
            let notification = Arc::new(Notification(AtomicU8::new(0)));
            node.callback.set_waker(notification.clone());
            #[cfg(feature = "iceoryx2")]
            state.events.extend(node.callback.take_iox2_events());
            let requested = node.schedule.run_on_start || node.callback.has_pending_inputs();
            let next = if node.schedule.run_on_start {
                Some(state.time)
            } else {
                node.schedule
                    .next_after(state.time)
                    .map_err(|source| StepError::Timing {
                        callback: node.name.clone(),
                        source,
                    })?
            };
            state.nodes.push(NodeState {
                requested,
                ready_since: None,
                busy_until: state.time,
                next,
            });
            state.notifications.push(notification);
            state.names.push(node.name);
            state.schedules.push(node.schedule);
            state.callbacks.push(node.callback);
        }
        Ok(state)
    }

    pub fn simulation_time(&self) -> FrameworkTime {
        self.time
    }
    pub fn current_time(&self) -> FrameworkTime {
        self.time
    }
    pub fn step_count(&self) -> Saturating<usize> {
        self.step_count
    }

    /// Actions at equal timestamps execute in insertion order, before selecting
    /// the callback batch. Scheduling in the past is rejected.
    pub fn schedule_at(
        &mut self,
        at: FrameworkTime,
        action: impl FnOnce(FrameworkTime) -> Result<(), String> + Send + 'storage,
    ) -> Result<(), StepError> {
        if self.failed {
            return Err(StepError::Poisoned);
        }
        if at < self.time {
            return Err(StepError::PastAction);
        }
        let sequence = self.action_sequence;
        self.action_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| StepError::InvalidConfig("action sequence exhausted".into()))?;
        self.actions.insert((at, sequence), Box::new(action));
        Ok(())
    }

    #[cfg(feature = "iceoryx2")]
    pub fn schedule_event(
        &mut self,
        at: FrameworkTime,
        channel: &str,
        id: iceoryx2::prelude::EventId,
        count: u64,
    ) -> Result<(), StepError> {
        let recipients: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.channel == channel)
            .map(|event| {
                (
                    event.staging.clone(),
                    event.wake.clone(),
                    event.observer.clone(),
                )
            })
            .collect();
        if recipients.is_empty() {
            return Err(StepError::UnknownEventChannel(channel.into()));
        }
        self.schedule_at(at, move |observed_at| {
            if count != 0 {
                for (queue, wake, observer) in recipients {
                    let record = task::iox2::EventRecord {
                        event_id: id,
                        count,
                    };
                    if let Some(observer) = observer {
                        observer(observed_at, record);
                    }
                    queue.push(record);
                    wake.wake();
                }
            }
            Ok(())
        })
    }

    /// Inject data without an implicit notification; schedule its event separately
    /// when replaying counted events so one input cannot generate two activations.
    #[cfg(feature = "iceoryx2")]
    pub fn schedule_iox2_input<
        T: std::fmt::Debug + iceoryx2::prelude::ZeroCopySend + Send + Sync + 'static,
    >(
        &mut self,
        at: FrameworkTime,
        publisher: Arc<std::sync::Mutex<task::iox2::Iox2Publisher<T>>>,
        value: T,
    ) -> Result<(), StepError> {
        self.schedule_at(at, move |time| {
            publisher
                .lock()
                .map_err(|e| e.to_string())?
                .publish_with_header(task::message::MessageHeader::new(time), value)
                .map_err(|e| format!("{e:?}"))
        })
    }

    pub fn step(&mut self) -> Result<StepResult, StepError> {
        if self.failed {
            return Err(StepError::Poisoned);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.step_inner()))
            .unwrap_or(Err(StepError::Panicked));
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub fn run_until_idle(&mut self, max_steps: usize) -> Result<Vec<StepResult>, StepError> {
        let mut steps = Vec::new();
        for _ in 0..max_steps {
            let step = self.step()?;
            let idle = step.idle;
            steps.push(step);
            if idle {
                return Ok(steps);
            }
        }
        Err(StepError::StepLimitExceeded)
    }

    fn poll_events(&self) -> Result<(), StepError> {
        #[cfg(feature = "iceoryx2")]
        if self.config.poll_external_events {
            for event in &self.events {
                let mut observed = false;
                event
                    .listener
                    .try_wait(|activation| {
                        let record = task::iox2::EventRecord {
                            event_id: activation.id,
                            count: activation.count,
                        };
                        if let Some(observer) = &event.observer {
                            observer(self.time, record);
                        }
                        event.staging.push(record);
                        observed = true;
                    })
                    .map_err(|e| StepError::Iox2Event(e.to_string()))?;
                if observed {
                    event.wake.wake();
                }
            }
        }
        Ok(())
    }

    fn refresh(&mut self) {
        for (index, node) in self.nodes.iter_mut().enumerate() {
            let notification = self.notifications[index].0.swap(0, Ordering::AcqRel);
            node.requested |=
                notification & TRIGGER != 0 || node.next.is_some_and(|next| next <= self.time);
            if node.requested
                && node.busy_until <= self.time
                && self.callbacks[index].required_inputs_available()
            {
                node.ready_since.get_or_insert(self.time);
            } else {
                node.ready_since = None;
            }
        }
    }

    fn free_threads(&self) -> Vec<usize> {
        let mut free = self.config.virtual_pool_threads.clone();
        for (node, schedule) in self.nodes.iter().zip(&self.schedules) {
            if node.busy_until > self.time {
                free[schedule.pool] -= 1;
            }
        }
        free
    }

    fn step_inner(&mut self) -> Result<StepResult, StepError> {
        let before = self.time;
        while self
            .actions
            .first_key_value()
            .is_some_and(|((at, _), _)| *at <= self.time)
        {
            let ((at, _), action) = self.actions.pop_first().unwrap();
            action(at).map_err(StepError::Action)?;
        }
        self.poll_events()?;
        self.refresh();
        let mut candidates: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| node.ready_since.map(|since| (since, index)))
            .collect();
        candidates.sort_unstable();
        let mut free = self.free_threads();
        let mut selected = Vec::new();
        for (_, index) in candidates {
            let pool = self.schedules[index].pool;
            if free[pool] != 0 {
                free[pool] -= 1;
                selected.push(index);
            }
        }
        let mut rank = vec![None; self.callbacks.len()];
        for (position, &index) in selected.iter().enumerate() {
            rank[index] = Some(position);
            self.nodes[index].ready_since = None;
        }
        let mut batch = Vec::new();
        for (index, callback) in self.callbacks.iter_mut().enumerate() {
            if let Some(position) = rank[index] {
                batch.push((position, index, callback.as_mut()));
            }
        }
        batch.sort_by_key(|(position, _, _)| *position);
        let (executed, timing) = execute_callback_batch(
            batch
                .into_iter()
                .map(|(_, index, callback)| (index, callback)),
            &self.metadata.context(self.time),
            self.config.node_executor_thread_count,
            |executed| {
                // Validate the whole batch's timing before publishing any outputs.
                let mut timing = Vec::with_capacity(executed.len());
                for &index in executed {
                    let finish = self
                        .time
                        .checked_add_duration(self.schedules[index].duration().map_err(
                            |source| StepError::Timing {
                                callback: self.names[index].clone(),
                                source,
                            },
                        )?)
                        .ok_or_else(|| StepError::Timing {
                            callback: self.names[index].clone(),
                            source: TimingError::Overflow,
                        })?;
                    let next = self.schedules[index].next_after(finish).map_err(|source| {
                        StepError::Timing {
                            callback: self.names[index].clone(),
                            source,
                        }
                    })?;
                    timing.push((finish, next));
                }
                Ok(timing)
            },
        )
        .map_err(|error| match error {
            BatchExecutionError::Callback { index, failure } => StepError::Callback {
                callback: self.names[index].clone(),
                failure,
            },
            BatchExecutionError::BeforeCommit(error) => error,
            BatchExecutionError::NoWorkers => {
                StepError::InvalidConfig("a callback batch requires a worker".into())
            }
        })?;
        for (&index, (finish, next)) in executed.iter().zip(timing) {
            self.nodes[index].requested = false;
            self.nodes[index].busy_until = finish;
            self.nodes[index].next = next;
        }
        self.poll_events()?;
        self.refresh();
        let free = self.free_threads();
        let runnable = self.nodes.iter().enumerate().any(|(index, node)| {
            node.ready_since.is_some() && free[self.schedules[index].pool] != 0
        });
        let next = self
            .nodes
            .iter()
            .flat_map(|node| [Some(node.busy_until), node.next])
            .flatten()
            .chain(self.actions.first_key_value().map(|((at, _), _)| *at))
            .filter(|&time| time > self.time)
            .min();
        if !runnable && let Some(next) = next {
            self.time = next;
        }
        self.refresh();
        let free = self.free_threads();
        let runnable = self.nodes.iter().enumerate().any(|(index, node)| {
            node.ready_since.is_some() && free[self.schedules[index].pool] != 0
        });
        let future = !self.actions.is_empty()
            || self.nodes.iter().any(|node| {
                node.busy_until > self.time || node.next.is_some_and(|next| next > self.time)
            });
        self.step_count += Saturating(1);
        Ok(StepResult {
            before,
            after: self.time,
            executed,
            idle: !runnable && !future,
        })
    }
}
