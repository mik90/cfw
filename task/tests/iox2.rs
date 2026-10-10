#![cfg(feature = "iceoryx2")]
use std::sync::atomic::{AtomicUsize, Ordering};
use task::iox2::{
    Iox2ChannelConfig, Iox2ChannelPlan, Iox2EventBindings, Iox2Notification, Iox2NotifyOutput,
    Iox2OptionalInput, Iox2Runtime,
};
use task::time::FrameworkTime;
use task::{ChannelPlan, GraphPlan, LoanError, StorageError};

fn name() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "cfw_task_ipc_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn unsent_and_cancelled_loans_release_capacity_and_success_stamps_headers() {
    let runtime = Iox2Runtime::new().unwrap();
    let mut plan = Iox2ChannelPlan::<u64>::new(name(), &runtime);
    let pub_key = plan.publisher(1);
    let sub_key = plan.subscriber(2);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build().unwrap();
    let mut publisher = bindings.take_publisher(&pub_key).unwrap();
    let subscriber = bindings.take_subscriber(&sub_key).unwrap();
    drop(bindings);
    drop(storage); // Ports retain the node and service resources independently.
    drop(runtime);
    drop(publisher.loan(1).unwrap());
    publisher.loan(2).unwrap().send();
    assert!(matches!(
        publisher.loan(3),
        Err(LoanError::LoanCapacityReached)
    ));
    publisher.discard_pending();
    let stamp = FrameworkTime::from_nanoseconds(77);
    publisher.loan(42).unwrap().send();
    publisher.flush(stamp);
    subscriber.update();
    let input = Iox2OptionalInput::new(&subscriber);
    assert_eq!(input.value(), Some(&42));
    assert_eq!(input.header().unwrap().published_at, stamp);
    assert_eq!(subscriber.receive_errors(), 0);
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn same_timestamp_ipc_batches_preserve_publisher_indices_and_send_order() {
    let runtime = Iox2Runtime::new().unwrap();
    let mut plan = Iox2ChannelPlan::<u64>::new(name(), &runtime);
    let first = plan.publisher_with_notification(2, Iox2Notification::Silent);
    let second = plan.publisher_with_notification(2, Iox2Notification::Silent);
    let capture = plan.subscriber(4);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build().unwrap();
    let mut first = bindings.take_publisher(&first).unwrap();
    let mut second = bindings.take_publisher(&second).unwrap();
    let capture = bindings.take_subscriber(&capture).unwrap();
    let stamp = FrameworkTime::from_nanoseconds(77);
    first.loan(10).unwrap().send();
    first.loan(11).unwrap().send();
    second.loan(20).unwrap().send();
    second.loan(21).unwrap().send();
    let mut predicted = Vec::new();
    first.visit_pending_headers(|mut h| {
        h.published_at = stamp;
        predicted.push(h);
    });
    second.visit_pending_headers(|mut h| {
        h.published_at = stamp;
        predicted.push(h);
    });
    first.flush(stamp);
    second.flush(stamp);
    capture.update();
    let mut actual = Vec::new();
    capture.inspect_messages(|_, m| actual.push((m.message, m.header)));
    actual.sort_by_key(|(value, _)| *value);
    assert_eq!(
        actual.iter().map(|(_, h)| *h).collect::<Vec<_>>(),
        predicted
    );
    assert_eq!(
        actual
            .iter()
            .map(|(_, h)| (h.publisher_index, h.batch_index))
            .collect::<Vec<_>>(),
        [(0, 0), (0, 1), (1, 0), (1, 1)]
    );
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn type_and_service_setting_mismatches_are_reported() {
    let runtime = Iox2Runtime::new().unwrap();
    let name = name();
    let mut first = Iox2ChannelPlan::<u64>::new(&name, &runtime);
    first.publisher(1);
    let storage = GraphPlan::new(first).allocate().unwrap();
    let mut wrong_type = Iox2ChannelPlan::<u32>::new(&name, &runtime);
    wrong_type.subscriber(1);
    assert!(matches!(
        GraphPlan::new(wrong_type).allocate(),
        Err(StorageError::Transport(_))
    ));
    let config = Iox2ChannelConfig {
        buffer_capacity: 32,
        max_borrowed_samples: 64,
        ..Default::default()
    };
    let mut incompatible = Iox2ChannelPlan::<u64>::new(&name, &runtime).with_config(config);
    incompatible.subscriber(1);
    assert!(matches!(
        GraphPlan::new(incompatible).allocate(),
        Err(StorageError::Transport(_))
    ));
    drop(storage);
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn native_and_ipc_channels_cannot_claim_the_same_name() {
    let runtime = Iox2Runtime::new().unwrap();
    let name = name();
    let native = ChannelPlan::<u64>::new(&name);
    let ipc = Iox2ChannelPlan::<u64>::new(&name, &runtime);
    assert!(matches!(
        GraphPlan::new((native, ipc)).allocate(),
        Err(StorageError::DuplicateChannel(_))
    ));
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn data_notification_policy_preserves_deferred_commit_and_silent_explicit_signalling() {
    struct NoWake;
    impl task::wake::Wake for NoWake {
        fn wake(&self) {}
    }

    for policy in [
        Iox2Notification::Silent,
        Iox2Notification::Event(0),
        Iox2Notification::Event(7),
    ] {
        let runtime = Iox2Runtime::new().unwrap();
        let mut plan =
            Iox2ChannelPlan::<u64>::new(name(), &runtime).with_config(Iox2ChannelConfig {
                event_id_max_value: 7,
                max_notifiers: if policy == Iox2Notification::Silent {
                    1
                } else {
                    2
                },
                ..Default::default()
            });
        let pub_key = plan.publisher(2);
        plan.set_publisher_notification(&pub_key, policy).unwrap();
        let sub_key = plan.subscriber(2);
        let events_key = plan.events(8);
        let signal_key = plan.notifier_with_id(3);
        let storage = GraphPlan::new(plan).allocate().unwrap();
        let bindings = storage.channels().build().unwrap();
        let mut publisher = bindings.take_publisher(&pub_key).unwrap();
        let subscriber = bindings.take_subscriber(&sub_key).unwrap();
        let mut signal = bindings.take_notifier(&signal_key).unwrap();
        let mut events = bindings.take_event(&events_key).unwrap();
        events.set_waker(std::sync::Arc::new(NoWake));
        let registration = events.take_registration().unwrap();
        let poll = || {
            let mut ids = Vec::new();
            registration
                .listener
                .try_wait(|event| ids.push(event.id.as_value()))
                .unwrap();
            ids
        };

        publisher.loan(10).unwrap().send();
        subscriber.update();
        assert!(Iox2OptionalInput::new(&subscriber).value().is_none());
        assert!(poll().is_empty());
        publisher.discard_pending();
        publisher.flush(FrameworkTime::from_nanoseconds(1));
        assert!(poll().is_empty());

        publisher.loan(42).unwrap().send();
        publisher.flush(FrameworkTime::from_nanoseconds(2));
        subscriber.update();
        assert_eq!(Iox2OptionalInput::new(&subscriber).value(), Some(&42));
        assert_eq!(
            Iox2OptionalInput::new(&subscriber)
                .header()
                .unwrap()
                .published_at
                .to_nanoseconds(),
            2
        );
        assert_eq!(
            poll(),
            match policy {
                Iox2Notification::Silent => vec![],
                Iox2Notification::Event(id) => vec![id],
            }
        );

        Iox2NotifyOutput::new(&mut signal).send();
        signal.flush(FrameworkTime::from_nanoseconds(2));
        assert_eq!(poll(), [3]);
        publisher.suppress_transport();
        publisher.loan(99).unwrap().send();
        publisher.flush(FrameworkTime::from_nanoseconds(3));
        subscriber.update();
        assert_eq!(Iox2OptionalInput::new(&subscriber).value(), Some(&42));
        assert!(poll().is_empty());
    }
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn notification_plan_checks_ids_keys_and_only_budgets_enabled_notifiers() {
    let runtime = Iox2Runtime::new().unwrap();
    let mut plan = Iox2ChannelPlan::<u64>::new(name(), &runtime);
    plan.publisher_with_notification(1, Iox2Notification::Event(1));
    assert!(matches!(
        GraphPlan::new(plan).allocate(),
        Err(StorageError::Transport(_))
    ));
    let mut plan = Iox2ChannelPlan::<u64>::new(name(), &runtime).with_config(Iox2ChannelConfig {
        max_notifiers: 1,
        ..Default::default()
    });
    let key = plan.publisher_with_notification(1, Iox2Notification::Silent);
    plan.publisher_with_notification(1, Iox2Notification::Silent);
    plan.notifier();
    let mut other = Iox2ChannelPlan::<u64>::new(name(), &runtime);
    assert_eq!(
        other.set_publisher_notification(&key, Iox2Notification::Event(0)),
        Err(task::EndpointError::WrongChannel)
    );
    let mut same_name = Iox2ChannelPlan::<u64>::new(plan.name(), &runtime);
    assert_eq!(
        same_name.set_publisher_notification(&key, Iox2Notification::Silent),
        Err(task::EndpointError::InvalidIndex(0))
    );
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let _bindings = storage.channels().build().unwrap();
}
