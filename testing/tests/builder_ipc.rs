#![cfg(feature = "iceoryx2")]
use iceoryx2::prelude::{EventId, ZeroCopySend};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use task::{
    RequiredInput,
    iox2::{Iox2Event, Iox2OptionalInput, Iox2Output, Iox2Runtime},
};
use task_macros::task_callback;
use testing::UnitTestExecutorBuilder;

fn channel() -> String {
    format!("builder_fixture_input_{}", std::process::id())
}
fn result() -> String {
    format!("builder_fixture_output_{}", std::process::id())
}
type Observations = Arc<Mutex<Vec<(u64, i64, Vec<(usize, u64)>)>>>;
struct Observe {
    observations: Observations,
}
#[repr(C)]
#[derive(Debug, Default, iceoryx2::prelude::ZeroCopySend)]
struct NonClonePayload {
    value: u64,
}
#[task_callback]
impl Observe {
    fn run(
        &self,
        #[channel(channel())] events: Iox2Event,
        #[channel(channel())] input: Iox2OptionalInput<u64>,
        #[trigger(false)] gate: RequiredInput<u64>,
        #[channel(result())] mut output: Iox2Output<NonClonePayload>,
    ) {
        self.observations.lock().unwrap().push((
            *input.value().unwrap(),
            input.header().unwrap().published_at.to_nanoseconds(),
            events
                .records()
                .map(|(id, count)| (id.as_value(), count))
                .collect(),
        ));
        output.value = input.value().unwrap() + *gate;
        output.send();
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn named_ipc_fixtures_event_first_binding_gating_and_send_timestamps() {
    let mut builder = UnitTestExecutorBuilder::new().with_iox2_runtime(Iox2Runtime::new().unwrap());
    let observations = Arc::new(Mutex::new(Vec::new()));
    builder.add_task(
        "observe",
        Observe {
            observations: observations.clone(),
        },
        Duration::from_nanos(10),
    );
    let mut input = builder.add_iox2_test_publisher::<u64>(&channel());
    let mut events = builder.add_iox2_test_notifier(&channel());
    let mut gate = builder.add_test_publisher::<u64>("gate");
    let output = builder.add_iox2_test_subscriber::<NonClonePayload>(&result());
    builder.run(|mut executor| {
        input.send(77);
        events.notify(EventId::new(9), 5);
        assert!(executor.step().executed.is_empty());
        gate.send(11);
        assert_eq!(executor.step().executed, [0]);
        assert_eq!(executor.current_time().to_nanoseconds(), 10);
        assert_eq!(
            output.messages(&mut executor, |i, m| assert_eq!(
                (i, m.message.value, m.header.published_at.to_nanoseconds()),
                (0, 88, 0)
            )),
            1
        );
        assert_eq!(
            output.messages(&mut executor, |_, _| panic!("already consumed")),
            0
        );
        assert!(
            executor.step().executed.is_empty(),
            "silent publication must not duplicate event notifications"
        );
        input.send(78);
        events.notify(EventId::new(4), 2);
        executor.step();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| output.messages(
                &mut executor,
                |i, m| {
                    assert_eq!(
                        (i, m.message.value, m.header.published_at.to_nanoseconds()),
                        (0, 89, 10)
                    );
                    panic!("inspection failed");
                }
            )))
            .is_err()
        );
        assert_eq!(
            output.messages(&mut executor, |_, _| panic!("panic must consume batch")),
            0
        );
    });
    assert_eq!(
        *observations.lock().unwrap(),
        [(77, 0, vec![(9, 5)]), (78, 10, vec![(4, 2)])]
    );
    assert!(input.try_send(0).is_err());
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn ipc_fixture_mismatches_fail_during_allocation() {
    for case in 0..4 {
        let mut builder = UnitTestExecutorBuilder::new();
        let configuration = builder.add_task(
            "observe",
            Observe {
                observations: Default::default(),
            },
            Duration::from_nanos(1),
        );
        if case == 3 {
            configuration.input_channel("input", "gate");
        }
        match case {
            0 => {
                builder.add_iox2_test_publisher::<u32>(&channel());
            }
            1 => {
                builder.add_test_publisher::<u64>(&channel());
            }
            2 => {
                builder.add_iox2_test_notifier("missing");
            }
            _ => {}
        }
        let error = builder.try_allocate().err().unwrap().to_string();
        assert!(
            error.contains(if case == 2 {
                "no task event subscriber"
            } else {
                "transport"
            }),
            "{error}"
        );
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn ipc_service_limits_cover_large_captures_and_explicit_limits_are_checked() {
    use task::{
        automatic::NamedPlan,
        iox2::{Iox2ChannelConfig, Iox2Notification},
    };
    let name = format!("named_limits_{}", std::process::id());
    let mut plan = NamedPlan::default();
    let publisher = plan
        .ipc::<u64>(&name)
        .unwrap()
        .publisher_with_notification(1, Iox2Notification::Silent);
    let subscriber = plan.ipc::<u64>(&name).unwrap().subscriber(1024);
    let limits = plan.ipc_service_limits().unwrap();
    assert_eq!(limits[&name].buffer_capacity, 1024);
    assert_eq!(limits[&name].max_borrowed_samples, 2048);
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let ports = bindings.ipc::<u64>(&name).unwrap();
    let publisher = ports.take_publisher(&publisher).unwrap();
    let subscriber = ports.take_subscriber(&subscriber).unwrap();
    for value in 0..1024 {
        publisher
            .publish_with_header(
                task::message::MessageHeader::new(task::time::FrameworkTime::from_nanoseconds(
                    value,
                )),
                value as u64,
            )
            .unwrap();
    }
    subscriber.update();
    assert_eq!(
        subscriber.inspect_messages(|index, message| assert_eq!(message.message, index as u64)),
        1024
    );

    let mut plan = NamedPlan::default();
    plan.ipc::<u64>("too-small").unwrap().subscriber(2048);
    assert_eq!(
        plan.ipc_service_limits().unwrap()["too-small"].buffer_capacity,
        2048
    );
    plan.set_ipc_service_limits("too-small", Iox2ChannelConfig::default());
    let error = plan.allocate().err().unwrap().to_string();
    assert!(
        error.contains("too-small") && error.contains("requires 2048, configured 1024"),
        "{error}"
    );
    let mut counts = NamedPlan::default();
    for _ in 0..20 {
        counts.ipc::<u64>("counts").unwrap().subscriber(1);
    }
    for _ in 0..10 {
        counts.ipc::<u64>("counts").unwrap().publisher(1);
    }
    counts.notifier("counts").unwrap();
    counts.event("counts", 1).unwrap();
    let limits = counts.ipc_service_limits().unwrap();
    assert_eq!(limits["counts"].max_subscribers, 20);
    assert_eq!(limits["counts"].max_publishers, 10);
    assert_eq!(limits["counts"].max_notifiers, 11);

    let mut builder = UnitTestExecutorBuilder::new();
    builder
        .add_task("feedback", Feedback, Duration::from_nanos(1))
        .input_channel("input", "limited")
        .output_channel("output", "limited");
    builder.add_iox2_test_publisher::<u64>("limited");
    builder.add_iox2_test_subscriber::<u64>("limited");
    builder.set_ipc_service_limits(
        "limited",
        Iox2ChannelConfig {
            buffer_capacity: 16,
            max_borrowed_samples: 32,
            ..Default::default()
        },
    );
    let error = builder.try_allocate().err().unwrap().to_string();
    assert!(error.contains("requires 1024, configured 16"), "{error}");
}

struct Feedback;
#[task_callback]
impl Feedback {
    fn run(&self, input: Iox2OptionalInput<u64>, mut output: Iox2Output<u64>) {
        if let Some(value) = input.value() {
            *output = value * 2;
            output.send();
        }
    }
}
#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn ipc_fixture_captures_distinguish_injection_from_feedback() {
    let name = format!("fixture_feedback_{}", std::process::id());
    let mut builder = UnitTestExecutorBuilder::new();
    builder
        .add_scheduled_task(
            "feedback",
            Feedback,
            Duration::from_nanos(1),
            task::CallbackSchedule::periodic(Duration::from_nanos(1)),
        )
        .input_channel("input", &name)
        .output_channel("output", &name);
    let mut input = builder.add_iox2_test_publisher::<u64>(&name);
    let output = builder.add_iox2_test_subscriber::<u64>(&name);
    let all = builder.add_iox2_test_subscriber_with_sources::<u64>(
        &name,
        1024,
        testing::CaptureSources::AllPublishers,
    );
    builder.run(|mut executor| {
        input.send(3);
        for _ in 0..2 {
            executor.step();
        }
        let mut outputs = Vec::new();
        output.messages(&mut executor, |_, message| outputs.push(message.message));
        assert!(!outputs.is_empty());
        assert!(outputs.iter().all(|value| *value >= 6));
        let mut seen = Vec::new();
        all.messages(&mut executor, |_, message| seen.push(message.message));
        assert!(seen.contains(&3));
        let mut count = 0;
        for _ in 0..3 {
            executor.step();
            count += output.messages(&mut executor, |_, message| assert!(message.message >= 12));
        }
        assert!(count > 0);
    });
}

struct Signal;
#[task_callback]
impl Signal {
    fn run(&self, notify: task::iox2::Iox2NotifyOutput) {
        notify.send();
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn overrides_bind_ipc_data_events_notifiers_and_native_gates() {
    let data_channel = format!("override_data_{}", std::process::id());
    let event_channel = format!("override_events_{}", std::process::id());
    let output_channel = format!("override_output_{}", std::process::id());
    let observations = Arc::new(Mutex::new(Vec::new()));
    let mut builder = UnitTestExecutorBuilder::new();
    builder
        .add_task(
            "observe",
            Observe {
                observations: observations.clone(),
            },
            Duration::from_nanos(3),
        )
        .input_channel("input", &data_channel)
        .input_channel("events", &event_channel)
        .input_channel("gate", "custom_gate")
        .output_channel("output", &output_channel);
    builder
        .add_scheduled_task(
            "signal",
            Signal,
            Duration::from_nanos(1),
            task::CallbackSchedule::on_start(),
        )
        .output_channel("notify", &event_channel);
    let mut input = builder.add_iox2_test_publisher::<u64>(&data_channel);
    let mut gate = builder.add_test_publisher::<u64>("custom_gate");
    let output = builder.add_iox2_test_subscriber::<NonClonePayload>(&output_channel);
    builder.run(|mut executor| {
        input.send(77);
        gate.send(11);
        assert_eq!(executor.step().executed, [1]);
        assert_eq!(executor.step().executed, [0]);
        assert_eq!(
            output.messages(&mut executor, |_, message| {
                assert_eq!(message.message.value, 88);
                assert_eq!(message.header.published_at.to_nanoseconds(), 1);
            }),
            1
        );
    });
    assert_eq!(*observations.lock().unwrap(), [(77, 0, vec![(0, 1)])]);
}
