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
    for case in 0..3 {
        let mut builder = UnitTestExecutorBuilder::new();
        builder.add_task(
            "observe",
            Observe {
                observations: Default::default(),
            },
            Duration::from_nanos(1),
        );
        match case {
            0 => {
                builder.add_iox2_test_publisher::<u32>(&channel());
            }
            1 => {
                builder.add_test_publisher::<u64>(&channel());
            }
            _ => {
                builder.add_iox2_test_notifier("missing");
            }
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
