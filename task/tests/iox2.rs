#![cfg(feature = "iceoryx2")]
use std::sync::atomic::{AtomicUsize, Ordering};
use task::iox2::{
    Iox2ChannelConfig, Iox2ChannelPlan, Iox2EventBindings, Iox2Notification, Iox2NotifyOutput,
    Iox2OptionalInput, Iox2Runtime,
};
use task::time::FrameworkTime;
use task::{LoanError, StorageError};

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
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let mut publisher = bindings.take_publisher(&pub_key).unwrap();
    let subscriber = bindings.take_subscriber(&sub_key).unwrap();
    drop(bindings);
    drop(storage); // Endpoints retain the node and service resources independently.
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
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
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
    let storage = first.allocate().unwrap();
    let mut wrong_type = Iox2ChannelPlan::<u32>::new(&name, &runtime);
    wrong_type.subscriber(1);
    assert!(matches!(
        wrong_type.allocate(),
        Err(StorageError::Transport(_))
    ));
    let config = Iox2ChannelConfig {
        buffer_capacity: 2048,
        max_borrowed_samples: 4096,
        ..Default::default()
    };
    let mut incompatible = Iox2ChannelPlan::<u64>::new(&name, &runtime).with_config(config);
    incompatible.subscriber(1);
    assert!(matches!(
        incompatible.allocate(),
        Err(StorageError::Transport(_))
    ));
    drop(storage);
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
                    3
                },
                ..Default::default()
            });
        let pub_key = plan.publisher(2);
        plan.set_publisher_notification(&pub_key, policy).unwrap();
        let local_key = plan.publisher_with_notification(2, policy);
        let sub_key = plan.subscriber(2);
        let events_key = plan.events(8);
        let signal_key = plan.notifier_with_id(3);
        let storage = plan.allocate().unwrap();
        let bindings = storage.bind().unwrap();
        bindings
            .set_output_delivery(1, false, task::iox2::OutputDelivery::Local)
            .unwrap();
        let mut local = bindings.take_publisher(&local_key).unwrap();
        assert_eq!(
            bindings.set_output_delivery(1, false, task::iox2::OutputDelivery::Transport),
            Err(task::EndpointError::AlreadyTaken)
        );
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
        local.loan(99).unwrap().send();
        local.flush(FrameworkTime::from_nanoseconds(3));
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
    assert!(matches!(plan.allocate(), Err(StorageError::Transport(_))));
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
    let storage = plan.allocate().unwrap();
    let _bindings = storage.bind().unwrap();
}

#[test]
#[cfg_attr(miri, ignore = "requires OS shared memory and IPC")]
fn split_data_and_event_plans_share_service_notifications() {
    struct NoWake;
    impl task::wake::Wake for NoWake {
        fn wake(&self) {}
    }

    let runtime = Iox2Runtime::new().unwrap();
    let channel = name();
    let config = Iox2ChannelConfig {
        buffer_capacity: 2,
        max_borrowed_samples: 4,
        max_publishers: 1,
        max_subscribers: 1,
        max_nodes: 1,
        max_listeners: 1,
        max_notifiers: 1,
        event_id_max_value: 0,
    };
    let mut data_plan = Iox2ChannelPlan::<u64>::new(channel.clone(), &runtime);
    data_plan.set_config(config.clone());
    let publisher_key = data_plan.publisher(2);
    let subscriber_key = data_plan.subscriber(2);
    let mut event_plan = Iox2ChannelPlan::<()>::new(channel, &runtime);
    event_plan.set_config(config);
    let event_key = event_plan.events(2);

    let data_storage = data_plan.allocate().unwrap();
    let event_storage = event_plan.allocate().unwrap();
    let data_bindings = data_storage.bind().unwrap();
    let event_bindings = event_storage.bind().unwrap();
    let mut publisher = data_bindings.take_publisher(&publisher_key).unwrap();
    let subscriber = data_bindings.take_subscriber(&subscriber_key).unwrap();
    let mut events = event_bindings.take_event(&event_key).unwrap();
    events.set_waker(std::sync::Arc::new(NoWake));
    let registration = events.take_registration().unwrap();

    publisher.loan(42).unwrap().send();
    publisher.flush(FrameworkTime::from_nanoseconds(1));
    let mut ids = Vec::new();
    registration
        .listener
        .try_wait(|event| ids.push(event.id.as_value()))
        .unwrap();
    assert_eq!(ids, [0]);
    subscriber.update();
    assert_eq!(Iox2OptionalInput::new(&subscriber).value(), Some(&42));
}
