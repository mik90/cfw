#![cfg(all(feature = "serde", feature = "iceoryx2"))]
use logging::{CapturePlan, FlushTrigger, LogFileWriter, LoggingScope};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use task::iox2::{Iox2ChannelPlan, Iox2EventBindings, Iox2NotifyOutput, Iox2Runtime};
use task::{ChannelPlan, GraphBuilder, GraphPlan, message::MessageHeader, time::FrameworkTime};

struct Writer(Arc<AtomicUsize>);
impl LogFileWriter for Writer {
    fn store_message(
        &mut self,
        _: &str,
        _: &MessageHeader,
        body: &[u8],
    ) -> Result<(), logging::BoxedLogError> {
        assert_eq!(body, b"42");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn write_artifact(&mut self, _: &str, _: &[u8]) -> Result<(), logging::BoxedLogError> {
        Ok(())
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and live workers")]
fn real_ipc_event_requests_a_flush_without_data_arrival_triggering_it() {
    let runtime = Iox2Runtime::new().unwrap();
    let name = format!("cfw_scheduled_flush_{}", std::process::id());
    let mut data = ChannelPlan::<u64>::new("data");
    let mut events = Iox2ChannelPlan::<()>::new(name, &runtime);
    let publisher = data.publisher(1);
    let capture = CapturePlan::declare(&mut data, 1);
    let trigger = events.events(4);
    let notifier = events.notifier();
    let storage = GraphPlan::new((data, events)).allocate().unwrap();
    let data = storage.channels().0.build();
    let events = storage.channels().1.build().unwrap();
    let mut publisher = data.take_publisher(&publisher).unwrap();
    let mut notifier = events.take_notifier(&notifier).unwrap();
    let messages = Arc::new(AtomicUsize::new(0));
    let logger = LoggingScope::new(
        Writer(messages.clone()),
        vec![capture.bind(&data).unwrap()],
        1,
    );
    let graph = logger
        .attach_event(
            GraphBuilder::with_storage(&storage).build().unwrap(),
            vec![FlushTrigger::ipc(events.take_event(&trigger).unwrap())],
        )
        .unwrap();
    publisher.publish(42).unwrap();
    publisher.flush(FrameworkTime::from_nanoseconds(0));
    live_executor::LiveExecutor::new(1, graph)
        .unwrap()
        .run_with(|stop| {
            assert_eq!(messages.load(Ordering::SeqCst), 0);
            Iox2NotifyOutput::new(&mut notifier).send();
            notifier.flush(FrameworkTime::from_nanoseconds(1));
            let start = std::time::Instant::now();
            while messages.load(Ordering::SeqCst) == 0 {
                assert!(start.elapsed() < Duration::from_secs(5));
                std::thread::yield_now();
            }
            stop.request_stop();
        })
        .unwrap();
    logger.finish().unwrap();
    assert_eq!(messages.load(Ordering::SeqCst), 1);
}
