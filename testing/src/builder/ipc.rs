use super::*;
use iceoryx2::prelude::{EventId, ZeroCopySend};
use std::fmt::Debug;
use task::iox2::{EventRecord, Iox2Publisher, Iox2PublisherKey, Iox2Subscriber, Iox2SubscriberKey};
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
}
struct IpcPump<T: Debug + ZeroCopySend + Send + Sync + 'static> {
    publisher: Iox2Publisher<T>,
    queue: Queue<T>,
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> InputPump for IpcPump<T> {
    fn flush(&mut self) -> Result<(), LoanError> {
        let batch = std::mem::take(&mut *self.queue.lock().unwrap());
        for (at, value) in batch {
            let mut header = task::message::MessageHeader::new(at);
            header.publisher_index = self.publisher.publisher_index();
            self.publisher.publish_with_header(header, value)?;
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
struct IpcInputPlan<T> {
    channel: String,
    queue: Queue<T>,
}
struct IpcInputFactory<T> {
    channel: String,
    queue: Queue<T>,
    key: Iox2PublisherKey<T>,
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> FixturePlan for IpcInputPlan<T> {
    fn declare(
        self: Box<Self>,
        plan: &mut NamedPlan,
    ) -> Result<Box<dyn FixtureFactory>, BuildError> {
        plan.require(&self.channel, false)?;
        let key = plan.ipc::<T>(&self.channel)?.publisher(1);
        Ok(Box::new(IpcInputFactory {
            channel: self.channel,
            queue: self.queue,
            key,
        }))
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> FixtureFactory for IpcInputFactory<T> {
    fn bind<'a>(
        self: Box<Self>,
        bindings: &NamedBindings<'a>,
        executor: &mut UnitTestExecutor<'a>,
    ) -> Result<(), BuildError> {
        executor.inputs.push(Box::new(IpcPump {
            publisher: bindings
                .ipc::<T>(&self.channel)?
                .take_publisher(&self.key)?,
            queue: self.queue,
        }));
        Ok(())
    }
}
struct IpcOutputPlan<T> {
    sources: CaptureSources,
    channel: String,
    capacity: usize,
    payload: PhantomData<T>,
}
struct IpcOutputFactory<T> {
    channel: String,
    key: Iox2SubscriberKey<T>,
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> FixturePlan for IpcOutputPlan<T> {
    fn declare(
        self: Box<Self>,
        plan: &mut NamedPlan,
    ) -> Result<Box<dyn FixtureFactory>, BuildError> {
        plan.require(&self.channel, true)?;
        let sources = plan.ipc_workload_publishers::<T>(&self.channel)?;
        let key = plan.ipc::<T>(&self.channel)?.subscriber(self.capacity);
        if self.sources == CaptureSources::TaskOutputs {
            plan.ipc::<T>(&self.channel)?
                .restrict_subscriber_sources(&key, &sources)?;
        }
        Ok(Box::new(IpcOutputFactory {
            channel: self.channel,
            key,
        }))
    }
}
impl<T: Debug + ZeroCopySend + Send + Sync + 'static> FixtureFactory for IpcOutputFactory<T> {
    fn bind<'a>(
        self: Box<Self>,
        bindings: &NamedBindings<'a>,
        executor: &mut UnitTestExecutor<'a>,
    ) -> Result<(), BuildError> {
        executor.captures.push(Box::new(
            bindings
                .ipc::<T>(&self.channel)?
                .take_subscriber(&self.key)?,
        ));
        Ok(())
    }
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
struct EventPlan {
    channel: String,
    queue: Queue<EventRecord>,
}
struct EventPump {
    channel: String,
    queue: Queue<EventRecord>,
    target: Arc<Mutex<Vec<(String, EventId, u64)>>>,
}
impl FixturePlan for EventPlan {
    fn declare(
        self: Box<Self>,
        plan: &mut NamedPlan,
    ) -> Result<Box<dyn FixtureFactory>, BuildError> {
        plan.require_event(&self.channel)?;
        Ok(self)
    }
}
impl FixtureFactory for EventPlan {
    fn bind<'a>(
        self: Box<Self>,
        _: &NamedBindings<'a>,
        executor: &mut UnitTestExecutor<'a>,
    ) -> Result<(), BuildError> {
        executor.inputs.push(Box::new(EventPump {
            channel: self.channel,
            queue: self.queue,
            target: executor.events.clone(),
        }));
        Ok(())
    }
}
impl InputPump for EventPump {
    fn flush(&mut self) -> Result<(), LoanError> {
        let batch = std::mem::take(&mut *self.queue.lock().unwrap());
        let mut target = self.target.lock().unwrap();
        for (_, record) in batch {
            target.push((self.channel.clone(), record.event_id, record.count));
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
        self.fixtures.push(Box::new(IpcInputPlan {
            channel: channel.into(),
            queue: queue.clone(),
        }));
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
        self.fixtures.push(Box::new(IpcOutputPlan::<T> {
            sources,
            channel: channel.into(),
            capacity,
            payload: PhantomData,
        }));
        TestOutput {
            index,
            session: self.session.0.clone(),
            payload: PhantomData,
        }
    }
    pub fn add_iox2_test_notifier(&mut self, channel: &str) -> TestNotifier {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        self.fixtures.push(Box::new(EventPlan {
            channel: channel.into(),
            queue: queue.clone(),
        }));
        TestNotifier {
            input: TestInput {
                queue,
                session: self.session.0.clone(),
            },
        }
    }
}
