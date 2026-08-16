use std::time::Duration;

use logging::InMemoryWriter;
use logging::log_task::{ChannelLogger, EVENT_CHANNEL, Event, EventLogTask};
use task::testing_subscriber::TestSubscriber;
use task::{CallbackBuilder, ChannelRegistry};
use testing::UnitTestExecutorBuilder;

const LOG_DIAGNOSTIC_CHANNEL_NAME: &str = "log_diagnostics";

#[test]
fn test_event_logging() {
    let channel_name = "my_channel".to_owned();

    let mut registry = ChannelRegistry::new();
    registry.register_channel::<u32>(channel_name.clone());
    let channel_logger = ChannelLogger::new(
        channel_name.clone(),
        registry
            .serializer_for(registry.channel_type(&channel_name).unwrap())
            .unwrap(),
    );

    let writer = Box::new(InMemoryWriter::new());
    let logged_data = writer.logged_data();
    let u32_subscriber = TestSubscriber::<u32>::new(channel_name.clone());

    let event_logging_task = EventLogTask::new(
        writer,
        LOG_DIAGNOSTIC_CHANNEL_NAME.into(),
        vec![channel_logger],
        vec![Box::new(u32_subscriber)],
        None,
    );

    let event_log_node = CallbackBuilder::new("EventLogTask".into(), Box::new(event_logging_task))
        .with_execution_duration_callback(|| Duration::from_millis(10))
        .build()
        .unwrap();

    let mut executor_builder = UnitTestExecutorBuilder::new(vec![event_log_node]);
    let mut event_publisher = executor_builder.add_test_publisher::<Event>(EVENT_CHANNEL);
    let mut u32_publisher = executor_builder.add_test_publisher::<u32>(&channel_name);
    let mut executor = executor_builder.build();

    // Publish a couple of values on the logged channel, then fire the event
    // that triggers the `EventLogTask` to drain and write them.
    u32_publisher.send(1);
    u32_publisher.send(2);
    event_publisher.send(Event {});

    executor.step();

    let logged = logged_data.lock().unwrap();
    let messages = logged.messages();
    assert_eq!(
        messages.len(),
        2,
        "both published u32 messages should be logged"
    );
    assert_eq!(messages[0].channel(), &channel_name);
    let first: u32 = serde_json::from_slice(messages[0].body()).unwrap();
    let second: u32 = serde_json::from_slice(messages[1].body()).unwrap();
    assert_eq!((first, second), (1, 2));
}
