#![cfg(feature = "iceoryx2")]
use iceoryx2::prelude::ZeroCopySend;
use logging::{AutomaticCapturePlan, CaptureOptions};
use std::sync::atomic::{AtomicUsize, Ordering};
use task::{
    CallbackSchedule, LoanError,
    automatic::{NamedPlan, TaskRegistration},
    iox2::{Iox2NotifyOutput, Iox2Output},
    loggable::{DeserializeError, Loggable, SerializeError},
    time::FrameworkTime,
};
use task_macros::task_callback;

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn automatic_ipc_replay_injects_full_headers_without_notifications() {
    use logging::{AutomaticReplayPlan, ReplayOptions};
    use task::iox2::Iox2EventBindings;
    struct NoWake;
    impl task::wake::Wake for NoWake {
        fn wake(&self) {}
    }
    let (data, event) = names();
    let mut plan = NamedPlan::default();
    let mut registration = TaskRegistration::new("ipc", Source, CallbackSchedule::default());
    registration
        .output_channel("output", &data)
        .output_channel("notify", &event);
    let _source = registration.register(&mut plan).unwrap();
    assert_eq!(
        plan.replayable_channels().collect::<Vec<_>>(),
        [data.as_str()]
    );
    let events = plan.ipc::<Wire>(&data).unwrap().events(4);
    let captures = AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(2)).unwrap();
    let replay = AutomaticReplayPlan::declare(&mut plan, [&data], &ReplayOptions::new(1)).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut listener = bindings
        .ipc::<Wire>(&data)
        .unwrap()
        .take_event(&events)
        .unwrap();
    listener.set_waker(std::sync::Arc::new(NoWake));
    let registration = listener.take_registration().unwrap();
    let mut sources = replay.bind(&bindings).unwrap();
    let mut captures = captures.bind(&bindings).unwrap();
    let header = task::message::MessageHeader {
        published_at: FrameworkTime::from_nanoseconds(17),
        publisher_index: 8,
        batch_index: 2,
    };
    sources[0].inject(header, &42_u64.to_le_bytes()).unwrap();
    assert_eq!(
        captures[0].drain_to_vec().unwrap(),
        [(header, 42_u64.to_le_bytes().to_vec())]
    );
    let mut events = Vec::new();
    registration
        .listener
        .try_wait(|event| events.push(event.id.as_value()))
        .unwrap();
    assert!(events.is_empty());
}

#[repr(C)]
#[derive(Debug, Default, ZeroCopySend)]
struct Wire(u64);
impl Loggable for Wire {
    type Context<'a> = ();
    fn serialize(&self, writer: &mut dyn std::io::Write) -> Result<(), SerializeError> {
        writer.write_all(&self.0.to_le_bytes())?;
        Ok(())
    }
    fn deserialize_with_ctx<'a>(bytes: &[u8], _: ()) -> Result<Self, DeserializeError>
    where
        Self: 'a,
    {
        Ok(Self(u64::from_le_bytes(bytes.try_into()?)))
    }
}
struct Source;
#[task_callback]
impl Source {
    fn run(&self, mut output: Iox2Output<Wire>, notify: Iox2NotifyOutput) -> Result<(), LoanError> {
        output.0 = 42;
        output.send();
        notify.send();
        Ok(())
    }
}
fn names() -> (String, String) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let prefix = format!(
        "cfw_auto_capture_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    (format!("{prefix}_data"), format!("{prefix}_event"))
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn named_ipc_data_is_discovered_and_captured_without_capturing_notifier_ports() {
    let (data, event) = names();
    let mut plan = NamedPlan::default();
    let mut registration = TaskRegistration::new("ipc", Source, CallbackSchedule::default());
    registration
        .output_channel("output", &data)
        .output_channel("notify", &event);
    let source = registration.register(&mut plan).unwrap();
    assert_eq!(
        plan.loggable_channels().collect::<Vec<_>>(),
        [data.as_str()]
    );
    let captures = AutomaticCapturePlan::declare(&mut plan, &CaptureOptions::new(1)).unwrap();
    assert!(plan.require(&data, false).is_err());
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut captures = captures.bind(&bindings).unwrap();
    let mut graph = storage.graph_builder();
    source.add_to_graph(&mut graph, &bindings);
    let mut graph = graph.build().unwrap();
    graph.step(FrameworkTime::from_nanoseconds(9)).unwrap();
    let messages = captures[0].drain_to_vec().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(Wire::deserialize(&messages[0].1).unwrap().0, 42);
    assert_eq!(
        messages[0].0.published_at,
        FrameworkTime::from_nanoseconds(9)
    );
    assert_eq!(
        (messages[0].0.publisher_index, messages[0].0.batch_index),
        (0, 0)
    );
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn ipc_capture_capacity_is_checked_by_storage_planning_and_exclusions_reserve_nothing() {
    for exclude in [false, true] {
        let (data, _) = names();
        let mut plan = NamedPlan::default();
        plan.ipc_publisher::<Wire>(&data, 1).unwrap();
        plan.set_ipc_service_limits(
            &data,
            task::iox2::Iox2ChannelConfig {
                buffer_capacity: 16,
                max_borrowed_samples: 32,
                ..Default::default()
            },
        );
        plan.register_loggable_ipc::<Wire>(&data).unwrap();
        let mut options = CaptureOptions::new(17);
        if exclude {
            options = options.exclude(&data);
        }
        let capture = AutomaticCapturePlan::declare(&mut plan, &options).unwrap();
        assert_eq!(capture.channels().is_empty(), exclude);
        assert_eq!(plan.allocate().is_ok(), exclude);
    }
}
