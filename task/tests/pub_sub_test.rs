use task::time::FrameworkTime;
use task::{CallbackNode, ChannelPlan, Context, GraphBuilder, GraphPlan, Publisher, Subscriber};

#[test]
fn executor_updates_inputs_and_flushes_hand_written_callback_outputs() {
    let mut source = ChannelPlan::<u32>::new("source");
    let source_pub = source.publisher(1);
    let source_sub = source.subscriber(1);
    let mut output = ChannelPlan::<u32>::new("output");
    let output_pub = output.publisher(1);
    let output_sub = output.subscriber(1);
    let storage = GraphPlan::new((source, output)).allocate().unwrap();
    let source = storage.channels().0.build();
    let output = storage.channels().1.build();
    let mut fixture = source.take_publisher(&source_pub).unwrap();
    let capture = output.take_subscriber(&output_sub).unwrap();
    let mut builder = GraphBuilder::new();
    builder.add_callback("increment", || {
        Ok(CallbackNode::new(
            source.take_subscriber(&source_sub)?,
            output.take_publisher(&output_pub)?,
            |input: &Subscriber<'_, u32>, output: &mut Publisher<'_, u32>, _ctx: &Context| {
                if let Some(value) = input.input().pop() {
                    output.publish(value.message + 1)?;
                }
                Ok(())
            },
        ))
    });
    let mut graph = builder.build().unwrap();
    fixture.publish(41).unwrap();
    fixture.flush(FrameworkTime::from_nanoseconds(1));
    graph.step(FrameworkTime::from_nanoseconds(2)).unwrap();
    capture.update();
    let retained = capture.input().pop().unwrap();
    drop((graph, source, output, fixture, capture));
    assert_eq!(retained.message, 42);
    assert_eq!(
        retained.header.published_at,
        FrameworkTime::from_nanoseconds(2)
    );
}
