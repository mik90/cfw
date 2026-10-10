#![cfg(feature = "serde")]
use logging::{CapturePlan, OwnedLogEntry, ReplayFeed, ReplaySourcePlan, SortedLogStreamReader};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use task::{ChannelPlan, GraphPlan, message::MessageHeader, time::FrameworkTime};

#[test]
fn paused_batch_preserves_cursor_without_claiming_eof_or_duplicating_inputs() {
    let mut plan = ChannelPlan::<u64>::new("input");
    let source = ReplaySourcePlan::declare(&mut plan, 1);
    let capture = CapturePlan::declare(&mut plan, 4);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let source = source
        .bind_with_decoder(&bindings, move |bytes| {
            let value: u64 = serde_json::from_slice(bytes)?;
            if value == 1 {
                flag.store(false, Ordering::Release);
            }
            Ok(value)
        })
        .unwrap();
    let time = FrameworkTime::from_nanoseconds(0);
    let entries = [
        ("input", b"1".as_slice()),
        ("ignored", b"ignored".as_slice()),
        ("input", b"2".as_slice()),
    ]
    .into_iter()
    .map(|(channel, bytes)| OwnedLogEntry {
        channel_name: channel.into(),
        header: MessageHeader::new(time),
        serialized_body: bytes.into(),
    })
    .collect();
    let reader = SortedLogStreamReader::from_entries(entries, HashMap::new()).unwrap();
    let mut feed = ReplayFeed::new(reader, [source], HashSet::from(["ignored".into()])).unwrap();
    assert!(
        feed.inject_due_while(time, |_, _, _| Ok(()), || running.load(Ordering::Acquire))
            .unwrap()
            .is_none()
    );
    assert!(!feed.exhausted());
    let mut capture = capture.bind(&bindings).unwrap();
    assert_eq!(capture.drain_to_vec().unwrap()[0].1, b"1");
    running.store(true, Ordering::Release);
    feed.inject_due_while(time, |_, _, _| Ok(()), || running.load(Ordering::Acquire))
        .unwrap();
    assert!(feed.exhausted());
    let messages = capture.drain_to_vec().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].1, b"2");
}
