pub use crate::builder::UnitTestExecutorBuilder;
pub use simulation_executor::{SimulationConfig as UnitTestExecutorConfig, StepResult};
use simulation_executor::{SimulationState, StepError};
use std::{
    cell::RefCell,
    num::Saturating,
    sync::{Arc, Mutex},
};
pub use task::testing_subscriber::{
    DEFAULT_TEST_SUBSCRIBER_CAPACITY, DroppedMessages, TestSubscriber,
};
use task::{Publisher, Subscriber, testing_time::TimeSource, time::FrameworkTime};
use task_graph::BuiltGraph;

/// Typed fixture publisher sharing the executor's clock and scheduled input queue.
pub struct TestPublisher<'storage, T> {
    publisher: Arc<Mutex<Publisher<'storage, T>>>,
    queue: std::sync::Weak<Mutex<std::collections::VecDeque<(FrameworkTime, T)>>>,
    time: Arc<TimeSource>,
}
impl<T> TestPublisher<'_, T> {
    pub fn try_send(&mut self, value: T) -> Result<(), task::LoanError> {
        let mut publisher = self.publisher.lock().unwrap();
        publisher.publish(value)?;
        publisher.flush(self.time.get());
        Ok(())
    }
    pub fn send(&mut self, value: T) {
        self.try_send(value).expect("test publication failed");
    }
    pub fn send_copied(&mut self, value: &T)
    where
        T: Clone,
    {
        self.send(value.clone());
    }
    pub fn try_send_at(
        &mut self,
        at: FrameworkTime,
        value: T,
    ) -> Result<(), crate::TestBuildError> {
        if at == FrameworkTime::INVALID || at < self.time.get() {
            return Err(crate::TestBuildError(
                "test publication requires a valid present or future time".into(),
            ));
        }
        let queue = self
            .queue
            .upgrade()
            .ok_or_else(|| crate::TestBuildError("test executor is closed".into()))?;
        queue.lock().unwrap().push_back((at, value));
        Ok(())
    }
    pub fn send_at(&mut self, at: FrameworkTime, value: T) {
        self.try_send_at(at, value)
            .expect("test input scheduling failed");
    }
}

#[cfg(feature = "iceoryx2")]
mod iox2;
#[cfg(feature = "iceoryx2")]
pub use iox2::{Iox2TestNotifier, Iox2TestPublisher, Iox2TestSubscriber};

/// Deterministic callback testing over externally owned graph storage.
/// Fixture endpoints must be reserved in channel plans before storage allocation.
/// Fixtures and retained native messages can outlive this executor, but not storage.
pub struct UnitTestExecutor<'storage> {
    simulation: SimulationState<'storage>,
    pub(crate) time: Arc<TimeSource>,
    pub(crate) inputs: Vec<Box<dyn crate::builder::InputPump<'storage> + 'storage>>,
    pub(crate) captures: Vec<Box<dyn crate::builder::Capture + 'storage>>,
    pub(crate) session: Option<crate::builder::SessionGuard>,
    failed: bool,
    #[cfg(feature = "iceoryx2")]
    pub(crate) events: iox2::PendingEvents,
}

impl<'storage> UnitTestExecutor<'storage> {
    pub fn new(graph: BuiltGraph<'storage>) -> Self {
        BoundUnitTestExecutorBuilder::new(graph).build()
    }
    pub fn new_with(graph: BuiltGraph<'storage>, config: UnitTestExecutorConfig) -> Self {
        BoundUnitTestExecutorBuilder::with_config(graph, config).build()
    }
    pub fn step(&mut self) -> StepResult {
        self.try_step().expect("could not step unit test executor")
    }
    pub fn try_step(&mut self) -> Result<StepResult, StepError> {
        if self.failed {
            return Err(StepError::Poisoned);
        }
        let result = self.step_inner();
        self.time.set(self.simulation.current_time());
        self.failed = result.is_err();
        if self.failed {
            self.inputs.clear();
        }
        if self.failed
            && let Some(session) = &self.session
        {
            session.close();
        }
        result
    }
    fn step_inner(&mut self) -> Result<StepResult, StepError> {
        for input in &mut self.inputs {
            input.schedule(&mut self.simulation)?;
        }
        #[cfg(feature = "iceoryx2")]
        for (channel, id, count) in std::mem::take(&mut *self.events.lock().unwrap()) {
            self.simulation
                .schedule_event(self.current_time(), &channel, id, count)?;
        }
        self.simulation.step()
    }
    pub fn step_count(&self) -> Saturating<usize> {
        self.simulation.step_count()
    }
    pub fn current_time(&self) -> FrameworkTime {
        self.simulation.current_time()
    }
    pub fn metadata(&self) -> &task_graph::GraphMetadata {
        self.simulation.metadata()
    }
    /// Schedule fixture work before ready callbacks at the given time.
    pub fn schedule_at(
        &mut self,
        at: FrameworkTime,
        action: impl FnOnce(FrameworkTime) -> Result<(), String> + Send + 'storage,
    ) -> Result<(), StepError> {
        if self.failed {
            return Err(StepError::Poisoned);
        }
        self.simulation.schedule_at(at, action)
    }
    pub fn run_until_idle(&mut self, max_steps: usize) -> Result<Vec<StepResult>, StepError> {
        let mut steps = Vec::new();
        for _ in 0..max_steps {
            let step = self.try_step()?;
            let idle = step.idle;
            steps.push(step);
            if idle {
                return Ok(steps);
            }
        }
        Err(StepError::StepLimitExceeded)
    }
    /// Execute work strictly before `until`, stopping at that simulated time.
    /// Work due exactly at the boundary is left for the next step.
    pub fn run_until(&mut self, until: FrameworkTime, max_steps: usize) -> Result<(), StepError> {
        if until == FrameworkTime::INVALID {
            return Err(StepError::InvalidConfig("invalid test deadline".into()));
        }
        self.schedule_at(until, |_| Ok(()))?;
        for _ in 0..max_steps {
            if self.current_time() == until {
                return Ok(());
            }
            self.try_step()?;
        }
        if self.current_time() == until {
            Ok(())
        } else {
            Err(StepError::StepLimitExceeded)
        }
    }
}

impl Drop for UnitTestExecutor<'_> {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            session.close();
        }
    }
}

/// Wrap a built graph and its already-bound fixture endpoints. Typed keys enforce
/// channel identity, payload type and single endpoint ownership during binding.
pub struct BoundUnitTestExecutorBuilder<'storage> {
    graph: BuiltGraph<'storage>,
    config: UnitTestExecutorConfig,
    time: Arc<TimeSource>,
    inputs: RefCell<Vec<Box<dyn crate::builder::InputPump<'storage> + 'storage>>>,
    #[cfg(feature = "iceoryx2")]
    events: iox2::PendingEvents,
}
impl<'storage> BoundUnitTestExecutorBuilder<'storage> {
    pub fn new(graph: BuiltGraph<'storage>) -> Self {
        Self::with_config(graph, UnitTestExecutorConfig::default())
    }
    pub fn with_config(graph: BuiltGraph<'storage>, config: UnitTestExecutorConfig) -> Self {
        Self {
            graph,
            time: Arc::new(TimeSource::new(config.start_time)),
            inputs: RefCell::new(Vec::new()),
            config,
            #[cfg(feature = "iceoryx2")]
            events: Default::default(),
        }
    }
    /// Apply an explicit test schedule to every callback in a low-level graph.
    pub fn schedule_all(mut self, schedule: task_graph::CallbackSchedule) -> Self {
        self.graph
            .configure_schedules(|_, target| *target = schedule.clone());
        self
    }
    pub fn start_time(mut self, at: FrameworkTime) -> Self {
        self.config.start_time = at;
        self.time.set(at);
        self
    }
    pub fn add_test_publisher<T: Send + Sync + 'storage>(
        &self,
        publisher: Publisher<'storage, T>,
    ) -> TestPublisher<'storage, T> {
        let publisher = Arc::new(Mutex::new(publisher));
        let queue: crate::builder::Queue<T> = Default::default();
        let handle = TestPublisher {
            publisher,
            queue: Arc::downgrade(&queue),
            time: self.time.clone(),
        };
        self.inputs
            .borrow_mut()
            .push(Box::new(crate::builder::NativePump {
                publisher: handle.publisher.clone(),
                queue,
            }));
        handle
    }
    pub fn add_test_subscriber<T>(
        &self,
        subscriber: Subscriber<'storage, T>,
    ) -> TestSubscriber<'storage, T> {
        TestSubscriber::new(subscriber)
    }
    pub fn build(self) -> UnitTestExecutor<'storage> {
        self.try_build()
            .expect("could not build unit test executor")
    }
    pub fn try_build(self) -> Result<UnitTestExecutor<'storage>, StepError> {
        Ok(UnitTestExecutor {
            simulation: SimulationState::with_config(self.graph, self.config)?,
            time: self.time,
            failed: false,
            inputs: self.inputs.into_inner(),
            captures: Vec::new(),
            session: None,
            #[cfg(feature = "iceoryx2")]
            events: self.events,
        })
    }
}
