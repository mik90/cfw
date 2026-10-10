use logging::{CapturePlan, ReplaySourcePlan};
use task::{
    ChannelPlan, GraphPlan, loggable::Loggable, message::MessageHeader, time::FrameworkTime,
};

struct Wire(u64);
impl Loggable for Wire {
    type Context<'a> = ();
    fn serialize(
        &self,
        writer: &mut dyn std::io::Write,
    ) -> Result<(), task::loggable::SerializeError> {
        writer.write_all(&self.0.to_le_bytes())?;
        Ok(())
    }
    fn deserialize_with_ctx<'a>(
        bytes: &[u8],
        _: (),
    ) -> Result<Self, task::loggable::DeserializeError>
    where
        Self: 'a,
    {
        Ok(Self(u64::from_le_bytes(
            bytes.try_into().map_err(|_| "expected eight bytes")?,
        )))
    }
}
#[test]
fn custom_codec_and_nonclone_payload_work_without_serde() {
    let mut plan = ChannelPlan::<Wire>::new("wire");
    let replay = ReplaySourcePlan::declare(&mut plan, 1);
    let capture = CapturePlan::declare(&mut plan, 1);
    let storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = storage.channels().build();
    let mut replay = replay.bind(&bindings).unwrap();
    let mut capture = capture.bind(&bindings).unwrap();
    let header = MessageHeader {
        published_at: FrameworkTime::from_nanoseconds(123),
        publisher_index: 7,
        batch_index: 3,
    };
    assert!(replay.inject(header, &[1]).is_err());
    replay.inject(header, &42_u64.to_le_bytes()).unwrap();
    let messages = capture.drain_to_vec().unwrap();
    assert_eq!(messages[0].0, header);
    assert_eq!(Wire::deserialize(&messages[0].1).unwrap().0, 42);
}

#[cfg(feature = "serde")]
#[test]
fn forwarded_decode_retains_source_arena_without_cloning_payload_or_owning_log() {
    use task::{
        ForwardedMessage,
        loggable::{MessageLog, ReplayMessageLog},
    };
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Source(String);
    let mut plan = ChannelPlan::new("source");
    let publisher = plan.publisher(1);
    let tap = plan.subscriber(1);
    let source_storage = GraphPlan::new(plan).allocate().unwrap();
    let source_bindings = source_storage.channels().build();
    let mut publisher = source_bindings.take_publisher(&publisher).unwrap();
    let tap = source_bindings.take_subscriber(&tap).unwrap();
    publisher.publish(Source("not Clone".into())).unwrap();
    publisher.flush(FrameworkTime::from_nanoseconds(5));
    tap.update();
    let source = tap.input().pop().unwrap();
    let forward = ForwardedMessage::new(true, source.clone());
    let mut encoded = Vec::new();
    forward.serialize(&mut encoded).unwrap();
    assert!(
        !String::from_utf8(encoded.clone())
            .unwrap()
            .contains("not Clone")
    );

    let mut plan = ChannelPlan::new("forward");
    let replay = ReplaySourcePlan::declare(&mut plan, 1);
    let capture = CapturePlan::declare(&mut plan, 1);
    let retained_key = plan.subscriber(1);
    let destination_storage = GraphPlan::new(plan).allocate().unwrap();
    let bindings = destination_storage.channels().build();
    let retained = bindings.take_subscriber(&retained_key).unwrap();
    {
        let log = ReplayMessageLog::new(vec![source.clone()]);
        let mut replay = replay
            .bind_with_decoder(&bindings, move |bytes| {
                ForwardedMessage::<bool, Source>::deserialize_with_ctx(bytes, &log)
            })
            .unwrap();
        replay
            .inject(
                MessageHeader::new(FrameworkTime::from_nanoseconds(10)),
                &encoded,
            )
            .unwrap();
    }
    assert_eq!(
        capture.bind(&bindings).unwrap().drain_to_vec().unwrap()[0].1,
        encoded
    );
    retained.update();
    let message = retained.input().pop().unwrap();
    assert_eq!(message.message.forwarded.message.0, "not Clone");
    assert!(message.message.message);
    let ambiguous = ReplayMessageLog::new(vec![source.clone(), source.clone()]);
    assert!(ambiguous.lookup(&source.header).is_none());
}
