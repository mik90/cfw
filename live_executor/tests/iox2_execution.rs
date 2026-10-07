#![cfg(feature = "iceoryx2")]
use crossbeam::channel::Sender;
use live_executor::LiveExecutor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use task::iox2::{Iox2ChannelConfig, Iox2NotifyOutput};
use task::iox2::{Iox2ChannelPlan, Iox2Event, Iox2OptionalInput, Iox2Runtime};
use task::time::FrameworkTime;
use task::{GraphBuilder, GraphPlan};
use task_macros::task_callback;

fn name() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "cfw_live_ipc_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

struct ReceiverTask {
    index: usize,
    received: Sender<(usize, u64, FrameworkTime, u64)>,
}
#[task_callback]
impl ReceiverTask {
    fn run(&mut self, input: Iox2OptionalInput<u64>, event: Iox2Event) {
        if let Some(&value) = input.value() {
            let count = event.events().map(|record| record.count).sum();
            self.received
                .send((
                    self.index,
                    value,
                    input.header().unwrap().published_at,
                    count,
                ))
                .unwrap();
        }
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn pre_start_notification_wakes_each_listener_without_periodic_polling() {
    let runtime = Iox2Runtime::new().unwrap();
    let mut plan = Iox2ChannelPlan::<u64>::new(name(), &runtime);
    let first = ReceiverTaskDeclaration::from_keys(plan.subscriber(1), plan.events(1));
    let second = ReceiverTaskDeclaration::from_keys(plan.subscriber(1), plan.events(1));
    let pub_key = plan.publisher(1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build().unwrap();
    let mut publisher = bindings.take_publisher(&pub_key).unwrap();
    let (sent, received) = crossbeam::channel::bounded(2);
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_callback("first", || {
        Ok(ReceiverTask {
            index: 0,
            received: sent.clone(),
        }
        .bind(first, &bindings, &bindings)?)
    });
    graph.add_callback("second", || {
        Ok(ReceiverTask {
            index: 1,
            received: sent.clone(),
        }
        .bind(second, &bindings, &bindings)?)
    });
    let graph = graph.build().unwrap();
    let stamp = FrameworkTime::from_nanoseconds(77);
    publisher.loan(42).unwrap().send();
    publisher.flush(stamp);
    let executor = LiveExecutor::new(2, graph).unwrap();
    let stop = executor.stop_signal();
    let mut messages = executor
        .run_with(|_| {
            (0..2)
                .map(|_| received.recv_timeout(Duration::from_secs(20)).unwrap())
                .collect::<Vec<_>>()
        })
        .unwrap();
    messages.sort_by_key(|m| m.0);
    assert_eq!(
        messages.iter().map(|m| (m.0, m.1, m.2)).collect::<Vec<_>>(),
        [(0, 42, stamp), (1, 42, stamp)]
    );
    assert!(messages.iter().all(|m| m.3 >= 1));
    assert!(stop.is_stopped());
    stop.request_stop();
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn controller_panic_wakes_and_joins_an_idle_readiness_thread() {
    let runtime = Iox2Runtime::new().unwrap();
    let mut plan = Iox2ChannelPlan::<u64>::new(name(), &runtime);
    let declaration = ReceiverTaskDeclaration::from_keys(plan.subscriber(1), plan.events(1));
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build().unwrap();
    let (sent, _) = crossbeam::channel::bounded(1);
    let mut graph = GraphBuilder::new();
    graph.add_callback("idle", || {
        Ok(ReceiverTask {
            index: 0,
            received: sent,
        }
        .bind(declaration, &bindings, &bindings)?)
    });
    let executor = LiveExecutor::new(2, graph.build().unwrap()).unwrap();
    let stop = executor.stop_signal();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        executor.run_with(|_| panic!("controller failed"))
    }));
    assert!(result.is_err());
    assert!(stop.is_stopped());
}

struct Notify;
#[task_callback]
impl Notify {
    fn run(&self, output: Iox2NotifyOutput) {
        output.send();
    }
}
struct Events {
    sent: Sender<usize>,
}
#[task_callback]
impl Events {
    fn run(&self, input: Iox2Event) {
        for record in input.events() {
            self.sent.send(record.event_id.as_value()).unwrap();
        }
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn standalone_notifier_dispatches_configured_event_id() {
    let runtime = Iox2Runtime::new().unwrap();
    let config = Iox2ChannelConfig {
        event_id_max_value: 3,
        ..Default::default()
    };
    let mut plan = Iox2ChannelPlan::<u64>::new(name(), &runtime).with_config(config);
    let notifier = NotifyDeclaration::from_keys(plan.notifier_with_id(3));
    let events = Events::declare(&mut plan).unwrap();
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build().unwrap();
    let (sent, received) = crossbeam::channel::bounded(1);
    let mut builder = GraphBuilder::new();
    builder.add_scheduled_callback("notify", task::CallbackSchedule::on_start(), || {
        Ok(Notify.bind(notifier, &bindings)?)
    });
    builder.add_callback("events", || Ok(Events { sent }.bind(events, &bindings)?));
    let executor = LiveExecutor::new(2, builder.build().unwrap()).unwrap();
    let id = executor
        .run_with(|_| received.recv_timeout(Duration::from_secs(20)).unwrap())
        .unwrap();
    assert_eq!(id, 3);
}

struct Gated {
    sent: Sender<u64>,
}
#[task_callback]
impl Gated {
    fn run(&self, mut required: task::RequiredInput<u64>, event: Iox2Event) {
        required.clear();
        self.sent.send(event.count()).unwrap();
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn event_is_retained_until_required_native_input_allows_execution() {
    struct Noop;
    impl task::wake::Wake for Noop {
        fn wake(&self) {}
    }
    let runtime = Iox2Runtime::new().unwrap();
    let mut event = Iox2ChannelPlan::<u64>::new(name(), &runtime);
    let mut input = task::ChannelPlan::<u64>::new("required");
    let declaration = Gated::declare(&mut input, &mut event).unwrap();
    let source_key = input.publisher(1);
    let storage = GraphPlan::new((input, event)).allocate().unwrap();
    let input = storage.channels().0.build();
    let event = storage.channels().1.build().unwrap();
    let mut source = input.take_publisher(&source_key).unwrap();
    let (sent, received) = crossbeam::channel::bounded(2);
    let mut callback = Gated { sent }.bind(declaration, &input, &event).unwrap();
    task::Callback::set_waker(&mut callback, std::sync::Arc::new(Noop));
    let registrations = task::Callback::take_iox2_events(&mut callback);
    registrations[0].staging.push(task::iox2::EventRecord {
        event_id: iceoryx2::prelude::EventId::new(0),
        count: 3,
    });
    let channels = Default::default();
    let callbacks = Default::default();
    let context = task::Context::new(FrameworkTime::from_nanoseconds(0), &channels, &callbacks);
    assert!(!task::execute_callback(&mut callback, &context).unwrap());
    for expected in [3, 0] {
        source.publish(1).unwrap();
        source.flush(context.now());
        assert!(task::execute_callback(&mut callback, &context).unwrap());
        assert_eq!(received.recv().unwrap(), expected);
    }
}
