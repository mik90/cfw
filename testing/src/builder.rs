//! High-level test construction with scoped, externally owned arena storage.
use std::{any::Any, cell::RefCell, collections::{HashSet, VecDeque}, marker::PhantomData, sync::{Arc, Mutex, atomic::{AtomicU8, Ordering}}, time::Duration};
use task::{automatic::{BuildError, NamedPlan, NamedStorage, NamedBindings, Task, TaskFactory}, CallbackSchedule, LoanError, Publisher, PublisherKey, SubscriberKey, SubscriberPolicy, message::Message, testing_time::TimeSource, time::FrameworkTime};
use crate::{BoundUnitTestExecutorBuilder, UnitTestExecutor, UnitTestExecutorConfig, TestSubscriber, DroppedMessages, DEFAULT_TEST_SUBSCRIBER_CAPACITY};

#[cfg(feature = "iceoryx2")]
mod ipc;
#[cfg(feature = "iceoryx2")]
pub use ipc::TestNotifier;

/// Required modeled duration. Zero is supported only when explicitly supplied.
pub enum ExecutionDuration {
    Fixed(Duration),
    Dynamic(Arc<dyn Fn() -> Duration + Send + Sync>),
}
impl From<Duration> for ExecutionDuration { fn from(value: Duration) -> Self { Self::Fixed(value) } }
impl ExecutionDuration {
    pub fn dynamic(f: impl Fn() -> Duration + Send + Sync + 'static) -> Self { Self::Dynamic(Arc::new(f)) }
    fn apply(self, schedule: CallbackSchedule) -> CallbackSchedule {
        match self {
            Self::Fixed(value) => schedule.with_execution_duration(value),
            Self::Dynamic(f) => schedule.with_execution_duration_callback(move || f()),
        }
    }
}

struct Session { phase: AtomicU8, time: Arc<TimeSource> }
impl Session {
    fn new() -> Arc<Self> { Arc::new(Self { phase: AtomicU8::new(0), time: Arc::new(TimeSource::new(FrameworkTime::from_nanoseconds(0))) }) }
    fn check(&self) -> Result<(), BuildError> {
        match self.phase.load(Ordering::Acquire) {
            1 => Ok(()),
            0 => Err(BuildError("test input is not active until build/run completes construction".into())),
            _ => Err(BuildError("test session is closed".into())),
        }
    }
}
pub(crate) struct SessionGuard(Arc<Session>);
impl Drop for SessionGuard { fn drop(&mut self) { self.0.phase.store(2, Ordering::Release); } }
type Queue<T> = Arc<Mutex<VecDeque<(FrameworkTime, T)>>>;

/// Owned injection handle, freely movable into the test closure. Values are
/// stamped on send and published at the next step's input boundary.
pub struct TestInput<T> { queue: Queue<T>, session: Arc<Session> }
impl<T> TestInput<T> {
    pub fn try_send(&mut self, value: T) -> Result<(), BuildError> {
        self.session.check()?;
        self.queue.lock().unwrap().push_back((self.session.time.get(), value));
        Ok(())
    }
    pub fn send(&mut self, value: T) { self.try_send(value).expect("test input send failed"); }
    pub fn send_copied(&mut self, value: &T) where T: Clone { self.send(value.clone()); }
}

/// Typed capture identity. Message references are borrowed only during inspection;
/// payloads need not be Clone and no arena reference is stored in this handle.
pub struct TestOutput<T> { index: usize, session: Arc<Session>, payload: PhantomData<fn(T) -> T> }
impl<T: 'static> TestOutput<T> {
    pub fn try_messages(&self, executor: &mut UnitTestExecutor<'_>, mut inspect: impl FnMut(usize, &Message<T>)) -> (usize, DroppedMessages) {
        assert!(executor.session.as_ref().is_some_and(|s| Arc::ptr_eq(&s.0, &self.session)), "test output belongs to another executor");
        executor.captures[self.index].messages(&mut |index, value| inspect(index, value.downcast_ref().expect("capture payload identity")))
    }
    pub fn messages(&self, executor: &mut UnitTestExecutor<'_>, inspect: impl FnMut(usize, &Message<T>)) -> usize {
        let (count, drops) = self.try_messages(executor, inspect);
        assert!(!drops.any(), "test output overflow: {drops:?}");
        count
    }
}
pub(crate) trait InputPump { fn flush(&mut self) -> Result<(), LoanError>; }
pub(crate) trait Capture {
    fn messages(&mut self, inspect: &mut dyn FnMut(usize, &dyn Any)) -> (usize, DroppedMessages);
}
impl<T: 'static> Capture for TestSubscriber<'_, T> {
    fn messages(&mut self, inspect: &mut dyn FnMut(usize, &dyn Any)) -> (usize, DroppedMessages) {
        self.try_messages(|i, message| inspect(i, message))
    }
}
struct NativePump<'a, T> { publisher: Publisher<'a, T>, queue: Queue<T> }
impl<T> InputPump for NativePump<'_, T> {
    fn flush(&mut self) -> Result<(), LoanError> {
        let batch = std::mem::take(&mut *self.queue.lock().unwrap());
        for (at, value) in batch { self.publisher.publish(value)?; self.publisher.flush(at); }
        Ok(())
    }
}
impl<T> Drop for NativePump<'_, T> { fn drop(&mut self) { self.queue.lock().unwrap_or_else(|p| p.into_inner()).clear(); } }

trait FixturePlan {
    fn declare(self: Box<Self>, plan: &mut NamedPlan) -> Result<Box<dyn FixtureFactory>, BuildError>;
}
trait FixtureFactory {
    fn bind<'a>(self: Box<Self>, bindings: &NamedBindings<'a>, executor: &mut UnitTestExecutor<'a>) -> Result<(), BuildError>;
}
struct InputPlan<T> { channel: String, queue: Queue<T> }
struct InputFactory<T> { channel: String, queue: Queue<T>, key: PublisherKey<T> }
impl<T: Send + Sync + 'static> FixturePlan for InputPlan<T> {
    fn declare(self: Box<Self>, plan: &mut NamedPlan) -> Result<Box<dyn FixtureFactory>, BuildError> {
        plan.require(&self.channel, false)?;
        let key = plan.native::<T>(&self.channel)?.publisher(1);
        Ok(Box::new(InputFactory { channel: self.channel, queue: self.queue, key }))
    }
}
impl<T: 'static> FixtureFactory for InputFactory<T> {
    fn bind<'a>(self: Box<Self>, bindings: &NamedBindings<'a>, executor: &mut UnitTestExecutor<'a>) -> Result<(), BuildError> {
        let publisher = bindings.native::<T>(&self.channel)?.take_publisher(&self.key)?;
        executor.inputs.push(Box::new(NativePump { publisher, queue: self.queue }));
        Ok(())
    }
}
struct OutputPlan<T> { channel: String, capacity: usize, payload: PhantomData<T> }
struct OutputFactory<T> { channel: String, key: SubscriberKey<T> }
impl<T: Send + Sync + 'static> FixturePlan for OutputPlan<T> {
    fn declare(self: Box<Self>, plan: &mut NamedPlan) -> Result<Box<dyn FixtureFactory>, BuildError> {
        plan.require(&self.channel, true)?;
        let key = plan.native::<T>(&self.channel)?.subscriber_with_policy(self.capacity, SubscriberPolicy { trigger: false, keep_across_runs: true });
        Ok(Box::new(OutputFactory { channel: self.channel, key }))
    }
}
impl<T: 'static> FixtureFactory for OutputFactory<T> {
    fn bind<'a>(self: Box<Self>, bindings: &NamedBindings<'a>, executor: &mut UnitTestExecutor<'a>) -> Result<(), BuildError> {
        executor.captures.push(Box::new(TestSubscriber::new(bindings.native::<T>(&self.channel)?.take_subscriber(&self.key)?)));
        Ok(())
    }
}

pub struct UnitTestExecutorBuilder {
    tasks: Vec<(String, CallbackSchedule, Box<dyn Task>)>,
    fixtures: Vec<Box<dyn FixturePlan>>,
    captures: usize,
    config: UnitTestExecutorConfig,
    session: SessionGuard,
    #[cfg(feature = "iceoryx2")]
    runtime: Option<Arc<task::iox2::Iox2Runtime>>,
}
impl Default for UnitTestExecutorBuilder { fn default() -> Self { Self::new() } }
impl UnitTestExecutorBuilder {
    pub fn new() -> Self { Self::with_config(UnitTestExecutorConfig::default()) }
    pub fn with_config(config: UnitTestExecutorConfig) -> Self {
        Self { tasks: Vec::new(), fixtures: Vec::new(), captures: 0, config, session: SessionGuard(Session::new()), #[cfg(feature = "iceoryx2")] runtime: None }
    }
    pub fn add_task(&mut self, name: impl Into<String>, task: impl Task + 'static, duration: impl Into<ExecutionDuration>) {
        self.add_scheduled_task(name, task, duration, CallbackSchedule::default());
    }
    pub fn add_scheduled_task(&mut self, name: impl Into<String>, task: impl Task + 'static, duration: impl Into<ExecutionDuration>, schedule: CallbackSchedule) {
        self.tasks.push((name.into(), duration.into().apply(schedule), Box::new(task)));
    }
    pub fn add_test_publisher<T: Send + Sync + 'static>(&mut self, channel: &str) -> TestInput<T> {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        self.fixtures.push(Box::new(InputPlan { channel: channel.into(), queue: queue.clone() }));
        TestInput { queue, session: self.session.0.clone() }
    }
    pub fn add_test_subscriber<T: Send + Sync + 'static>(&mut self, channel: &str) -> TestOutput<T> {
        self.add_test_subscriber_with_capacity(channel, DEFAULT_TEST_SUBSCRIBER_CAPACITY)
    }
    pub fn add_test_subscriber_with_capacity<T: Send + Sync + 'static>(&mut self, channel: &str, capacity: usize) -> TestOutput<T> {
        let index = self.captures;
        self.captures += 1;
        self.fixtures.push(Box::new(OutputPlan::<T> { channel: channel.into(), capacity, payload: PhantomData }));
        TestOutput { index, session: self.session.0.clone(), payload: PhantomData }
    }
    pub fn allocate(self) -> UnitTestSetup { self.try_allocate().expect("could not allocate unit test") }
    pub fn try_allocate(self) -> Result<UnitTestSetup, BuildError> {
        let mut plan = NamedPlan::default();
        #[cfg(feature = "iceoryx2")]
        if let Some(runtime) = self.runtime { plan = NamedPlan::with_iox2_runtime(runtime); }
        let mut factories = Vec::new();
        let mut names = HashSet::new();
        for (name, schedule, task) in self.tasks {
            if !names.insert(name.clone()) { return Err(BuildError(format!("duplicate task '{name}'"))); }
            let factory = task.register(&mut plan).map_err(|e| BuildError(format!("task '{name}': {e}")))?;
            factories.push((name, schedule, factory));
        }
        let fixtures = self.fixtures.into_iter().map(|f| f.declare(&mut plan)).collect::<Result<Vec<_>, _>>()?;
        Ok(UnitTestSetup { storage: plan.allocate()?, definition: RefCell::new(Some(Definition { factories, fixtures, config: self.config })), session: self.session })
    }
    pub fn run<R>(self, test: impl for<'a> FnOnce(UnitTestExecutor<'a>) -> R) -> R {
        self.try_run(test).expect("could not construct unit test")
    }
    pub fn try_run<R>(self, test: impl for<'a> FnOnce(UnitTestExecutor<'a>) -> R) -> Result<R, BuildError> {
        let setup = self.try_allocate()?;
        let executor = setup.try_build()?;
        Ok(test(executor))
    }
}
struct Definition {
    factories: Vec<(String, CallbackSchedule, Box<dyn TaskFactory>)>,
    fixtures: Vec<Box<dyn FixtureFactory>>,
    config: UnitTestExecutorConfig,
}
/// Owns arena storage independently of the single-use execution definition.
/// Ordinary scope cleanup suffices; retained low-level messages must not outlive
/// the storage owner.
pub struct UnitTestSetup {
    storage: NamedStorage,
    definition: RefCell<Option<Definition>>,
    session: SessionGuard,
}
impl UnitTestSetup {
    pub fn build(&self) -> UnitTestExecutor<'_> { self.try_build().expect("could not build unit test") }
    pub fn try_build(&self) -> Result<UnitTestExecutor<'_>, BuildError> {
        let definition = self.definition.borrow_mut().take().ok_or_else(|| BuildError("unit test setup has already been built".into()))?;
        let guard = SessionGuard(self.session.0.clone());
        let bindings = self.storage.bind()?;
        let mut graph = self.storage.graph_builder();
        for (name, schedule, factory) in definition.factories {
            let bindings = &bindings;
            graph.add_boxed_callback(name, schedule, move || factory.build(bindings).map_err(|e| Box::new(e) as task::FactoryError));
        }
        let graph = graph.build().map_err(|e| BuildError(format!("{e:?}")))?;
        let start_time = definition.config.start_time;
        let mut executor = BoundUnitTestExecutorBuilder::with_config(graph, definition.config).try_build().map_err(|e| BuildError(e.to_string()))?;
        for fixture in definition.fixtures { fixture.bind(&bindings, &mut executor)?; }
        executor.time = self.session.0.time.clone();
        executor.time.set(start_time);
        self.session.0.phase.store(1, Ordering::Release);
        executor.session = Some(guard);
        Ok(executor)
    }
}
