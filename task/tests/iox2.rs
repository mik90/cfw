#![cfg(feature = "iceoryx2")]
use std::sync::atomic::{AtomicUsize, Ordering};
use task::iox2::{Iox2ChannelConfig, Iox2ChannelPlan, Iox2OptionalInput, Iox2Runtime};
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
