pub use crate::builder::UnitTestExecutorBuilder;
pub use simulation_executor::{SimulationConfig as UnitTestExecutorConfig, StepResult};
use simulation_executor::{SimulationState, StepError};
use std::{num::Saturating, sync::Arc};
pub use task::testing_publisher::TestPublisher;
pub use task::testing_subscriber::{
    DEFAULT_TEST_SUBSCRIBER_CAPACITY, DroppedMessages, TestSubscriber,
};
use task::{BuiltGraph, Publisher, Subscriber, testing_time::TimeSource, time::FrameworkTime};

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
    pub(crate) inputs: Vec<Box<dyn crate::builder::InputPump + 'storage>>,
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
        if self.failed
            && let Some(session) = &self.session
        {
            session.close();
        }
        result
    }
    fn step_inner(&mut self) -> Result<StepResult, StepError> {
        for input in &mut self.inputs {
            input
                .flush()
                .map_err(|e| StepError::Action(format!("test input: {e:?}")))?;
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
            config,
            #[cfg(feature = "iceoryx2")]
            events: Default::default(),
        }
    }
    pub fn add_test_publisher<T>(
        &self,
        publisher: Publisher<'storage, T>,
    ) -> TestPublisher<'storage, T> {
        TestPublisher::new(publisher, self.time.clone())
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
            inputs: Vec::new(),
            captures: Vec::new(),
            session: None,
            #[cfg(feature = "iceoryx2")]
            events: self.events,
        })
    }
}
