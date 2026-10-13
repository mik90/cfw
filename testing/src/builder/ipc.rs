use super::*;
use iceoryx2::prelude::{EventId, ZeroCopySend};
use std::fmt::Debug;
use task::iox2::{EventRecord, Iox2Publisher, Iox2Subscriber};
pub const DEFAULT_IPC_TEST_SUBSCRIBER_CAPACITY: usize = 1024;

pub struct TestNotifier {
    input: TestInput<EventRecord>,
}
impl TestNotifier {
    pub fn try_notify(&mut self, event_id: EventId, count: u64) -> Result<(), BuildError> {
        self.input.try_send(EventRecord { event_id, count })
    }
    pub fn notify(&mut self, event_id: EventId, count: u64) {
        self.try_notify(event_id, count)
            .expect("test event notification failed");
    }
    pub fn try_notify_at(
        &mut self,
        at: FrameworkTime,
        event_id: EventId,
        count: u64,
    ) -> Result<(), BuildError> {
        self.input.try_send_at(at, EventRecord { event_id, count })
    }
    pub fn notify_at(&mut self, at: FrameworkTime, event_id: EventId, count: u64) {
        self.try_notify_at(at, event_id, count)
            .expect("test event scheduling failed");
    }
}
struct IpcPump<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    publisher: Arc<Mutex<Iox2Publisher<T>>>,
    queue: Queue<T>,
}
impl<'a, T: Debug + ZeroCopySend + Send + Sync + 'static> InputPump<'a> for IpcPump<T> {
    fn schedule(
        &mut self,
        simulation: &mut simulation_executor::SimulationState<'a>,
    ) -> Result<(), simulation_executor::StepError> {
        let batch = std::mem::take(&mut *self.queue.lock().unwrap());
        for (at, value) in batch {
            simulation.schedule_iox2_input(at, self.publisher.clone(), value)?;
        }
        Ok(())
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Drop for IpcPump<T> {
    fn drop(&mut self) {
        let pending = std::mem::take(&mut *self.queue.lock().unwrap_or_else(|p| p.into_inner()));
        drop(pending);
    }
}
fn input_fixture<T: Debug + ZeroCopySend + Send + Sync + 'static>(
    channel: String,
    queue: Queue<T>,
) -> FixtureDeclaration {
    Box::new(move |plan| {
        plan.require(&channel, false)?;
        let key = plan.ipc::<T>(&channel)?.publisher(1);
        Ok(Box::new(move |bindings, executor| {
            executor.inputs.push(Box::new(IpcPump {
                publisher: Arc::new(Mutex::new(
                    bindings.ipc::<T>(&channel)?.take_publisher(&key)?,
                )),
                queue,
            }));
            Ok(())
        }))
    })
}
fn output_fixture<T: Debug + ZeroCopySend + Send + Sync + 'static>(
    channel: String,
    capacity: usize,
    capture_sources: CaptureSources,
) -> FixtureDeclaration {
    Box::new(move |plan| {
        plan.require(&channel, true)?;
        let sources = plan.ipc_workload_publishers::<T>(&channel)?;
        let key = plan.ipc::<T>(&channel)?.subscriber(capacity);
        if capture_sources == CaptureSources::TaskOutputs {
            plan.ipc::<T>(&channel)?
                .restrict_subscriber_sources(&key, &sources)?;
        }
        Ok(Box::new(move |bindings, executor| {
            executor.captures.push(Box::new(
                bindings.ipc::<T>(&channel)?.take_subscriber(&key)?,
            ));
            Ok(())
        }))
    })
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> Capture for Iox2Subscriber<T> {
    fn messages(&mut self, inspect: &mut dyn FnMut(usize, &dyn Any)) -> (usize, DroppedMessages) {
        self.update();
        assert_eq!(self.receive_errors(), 0, "IPC capture receive failed");
        (
            self.inspect_messages(|i, m| inspect(i, m)),
            DroppedMessages::default(),
        )
    }
}
struct EventPump {
    channel: String,
    queue: Queue<EventRecord>,
}
impl<'a> InputPump<'a> for EventPump {
    fn schedule(
        &mut self,
        simulation: &mut simulation_executor::SimulationState<'a>,
    ) -> Result<(), simulation_executor::StepError> {
        let batch = std::mem::take(&mut *self.queue.lock().unwrap());
        for (at, record) in batch {
            simulation.schedule_event(at, &self.channel, record.event_id, record.count)?;
        }
        Ok(())
    }
}
impl UnitTestExecutorBuilder {
    pub fn set_ipc_service_limits(
        &mut self,
        channel: impl Into<String>,
        limits: task::iox2::Iox2ChannelConfig,
    ) -> &mut Self {
        self.ipc_limits.insert(channel.into(), limits);
        self
    }
    pub fn with_iox2_runtime(mut self, runtime: Arc<task::iox2::Iox2Runtime>) -> Self {
        self.runtime = Some(runtime);
        self
    }
    pub fn add_iox2_test_publisher<T: Debug + ZeroCopySend + Send + Sync + 'static>(
        &mut self,
        channel: &str,
    ) -> TestInput<T> {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        self.fixtures
            .push(input_fixture(channel.into(), queue.clone()));
        TestInput {
            queue,
            session: self.session.0.clone(),
        }
    }
    pub fn add_iox2_test_subscriber<T: Debug + ZeroCopySend + Send + Sync + 'static>(
        &mut self,
        channel: &str,
    ) -> TestOutput<T> {
        self.add_iox2_test_subscriber_with_capacity(channel, DEFAULT_IPC_TEST_SUBSCRIBER_CAPACITY)
    }
    pub fn add_iox2_test_subscriber_with_capacity<
        T: Debug + ZeroCopySend + Send + Sync + 'static,
    >(
        &mut self,
        channel: &str,
        capacity: usize,
    ) -> TestOutput<T> {
        self.add_iox2_test_subscriber_with_sources(channel, capacity, CaptureSources::TaskOutputs)
    }
    pub fn add_iox2_test_subscriber_with_sources<
        T: Debug + ZeroCopySend + Send + Sync + 'static,
    >(
        &mut self,
        channel: &str,
        capacity: usize,
        sources: CaptureSources,
    ) -> TestOutput<T> {
        let index = self.captures;
        self.captures += 1;
        self.fixtures
            .push(output_fixture::<T>(channel.into(), capacity, sources));
        TestOutput {
            index,
            session: self.session.0.clone(),
            payload: PhantomData,
        }
    }
    pub fn add_iox2_test_notifier(&mut self, channel: &str) -> TestNotifier {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let channel = channel.to_owned();
        let pending = queue.clone();
        self.fixtures.push(Box::new(move |plan| {
            plan.require_event(&channel)?;
            Ok(Box::new(move |_, executor| {
                executor.inputs.push(Box::new(EventPump {
                    channel,
                    queue: pending,
                }));
                Ok(())
            }))
        }));
        TestNotifier {
            input: TestInput {
                queue,
                session: self.session.0.clone(),
            },
        }
    }
}
