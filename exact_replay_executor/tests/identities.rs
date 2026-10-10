use exact_replay_executor::{
    ExactReplayExecutor, ReplayBindings, ReplayInputPlan, ReplayLog, SourceCachePlan,
};
use logging::{CapturePlan, ExecutionRecorder, LogSession, PortCapture};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use task::{
    ChannelPlan, ForwardedMessage, GraphBuilder, GraphPlan, Input, InputSpan, LoanError,
    OutputSpan, Publisher, time::FrameworkTime,
};
use task_macros::task_callback;

#[derive(Clone, Default)]
struct Bytes(Arc<Mutex<Vec<u8>>>);
impl Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Value(u64);
struct Source(u64);
#[task_callback]
impl Source {
    fn run(&mut self, #[capacity(2)] output: OutputSpan<Value>) -> Result<(), LoanError> {
        let first = output.loan(Value(self.0))?;
        let second = output.loan(Value(self.0 + 1))?;
        second.send();
        first.send();
        self.0 += 2;
        Ok(())
    }
}
struct Forward;
#[task_callback]
impl Forward {
    fn run<'storage>(
        &self,
        #[capacity(8)] mut input: Input<'_, 'storage, Value>,
        #[capacity(4)] output: &mut Publisher<'storage, ForwardedMessage<'storage, bool, Value>>,
    ) -> Result<(), LoanError> {
        for value in input.drain() {
            output.publish(ForwardedMessage::new(true, value))?;
        }
        Ok(())
    }
}
struct Observe;
#[task_callback]
impl Observe {
    fn run(
        &self,
        #[capacity(8)] input: InputSpan<Value>,
        output: &mut Publisher<Vec<u64>>,
    ) -> Result<(), LoanError> {
        output.publish(input.inputs().map(|m| m.message.0).collect())
    }
}
struct Consume;
#[task_callback]
impl Consume {
    fn run(
        &self,
        #[capacity(8)] input: InputSpan<ForwardedMessage<'_, bool, Value>>,
        output: &mut Publisher<Vec<u64>>,
    ) -> Result<(), LoanError> {
        output.publish(
            input
                .inputs()
                .map(|m| m.message.forwarded.message.0)
                .collect(),
        )
    }
}

fn replay_batches(logged_source: bool, logged_forward: bool) {
    let bytes = Bytes::default();
    {
        let mut source = ChannelPlan::new("source");
        let mut forward = ChannelPlan::new("forward");
        let mut observed = ChannelPlan::new("observed");
        let mut result = ChannelPlan::new("result");
        let a = Source::declare(&mut source).unwrap();
        let b = Source::declare(&mut source).unwrap();
        let f = Forward::declare(&mut source, &mut forward).unwrap();
        let o = Observe::declare(&mut source, &mut observed).unwrap();
        let c = Consume::declare(&mut forward, &mut result).unwrap();
        for key in [a.output_key(), b.output_key()] {
            source.reserve_retained(key, 8).unwrap();
        }
        let source_log = logged_source.then(|| CapturePlan::declare(&mut source, 8));
        let forward_log = logged_forward.then(|| CapturePlan::declare(&mut forward, 8));
        let observed_log = CapturePlan::declare(&mut observed, 2);
        let result_log = CapturePlan::declare(&mut result, 2);
        let storage = GraphPlan::new((source, (forward, (observed, result))))
            .allocate()
            .unwrap();
        let source = storage.channels().0.build();
        let forward = storage.channels().1.0.build();
        let observed = storage.channels().1.1.0.build();
        let result = storage.channels().1.1.1.build();
        let mut graph = GraphBuilder::with_storage(&storage);
        graph.add_callback("a", || Ok(Source(10).bind(a, &source)?));
        graph.add_callback("b", || Ok(Source(20).bind(b, &source)?));
        graph.add_callback("forward", || Ok(Forward.bind(f, &source, &forward)?));
        graph.add_callback("observe", || Ok(Observe.bind(o, &source, &observed)?));
        graph.add_callback("consume", || Ok(Consume.bind(c, &forward, &result)?));
        let recorder = ExecutionRecorder::new(10);
        let mut graph = recorder.attach(graph.build().unwrap()).unwrap();
        let mut captures = vec![
            observed_log.bind(&observed).unwrap(),
            result_log.bind(&result).unwrap(),
        ];
        if let Some(log) = source_log {
            captures.push(log.bind(&source).unwrap());
        }
        if let Some(log) = forward_log {
            captures.push(log.bind(&forward).unwrap());
        }
        let session = LogSession::new(
            logging::log_file_json::JsonLogFileWriter::new(bytes.clone()),
            captures,
        )
        .with_recording(recorder.clone())
        .unwrap();
        graph.step(FrameworkTime::from_nanoseconds(0)).unwrap();
        graph.step(FrameworkTime::from_nanoseconds(10)).unwrap();
        let descriptor = recorder.descriptor().unwrap();
        assert_eq!(
            descriptor.callbacks[0].endpoints[0].publisher_index,
            Some(0)
        );
        assert_eq!(
            descriptor.callbacks[1].endpoints[0].publisher_index,
            Some(1)
        );
        session.finish().unwrap();
    }
    let reader =
        logging::log_file_json::JsonLogFileReader::from_reader(bytes.0.lock().unwrap().as_slice())
            .unwrap();
    let log = ReplayLog::from_reader(&reader).unwrap();
    assert_eq!(log.source_capacity("source"), 8);
    assert_eq!(log.source_capacity("forward"), 8);

    let mut source = ChannelPlan::new("source");
    let mut forward = ChannelPlan::new("forward");
    let mut observed = ChannelPlan::new("observed");
    let mut result = ChannelPlan::new("result");
    // Extra replay-only publishers and reversed declarations change storage indices.
    let cache = SourceCachePlan::declare(&mut source, log.source_capacity("source")).unwrap();
    let b = Source::declare(&mut source).unwrap();
    let a = Source::declare(&mut source).unwrap();
    let f = Forward::declare(&mut source, &mut forward).unwrap();
    let o = Observe::declare(&mut source, &mut observed).unwrap();
    let c = Consume::declare(&mut forward, &mut result).unwrap();
    let hydrate_f = ReplayInputPlan::declare(&mut source, f.input_key()).unwrap();
    let hydrate_o = ReplayInputPlan::declare(&mut source, o.input_key()).unwrap();
    let hydrate_c = ReplayInputPlan::declare(&mut forward, c.input_key()).unwrap();
    let storage = GraphPlan::new((source, (forward, (observed, result))))
        .allocate()
        .unwrap();
    let source = storage.channels().0.build();
    let forward = storage.channels().1.0.build();
    let observed = storage.channels().1.1.0.build();
    let result = storage.channels().1.1.1.build();
    let cache = cache.bind(&source).unwrap();
    let captured_indices = Arc::new(Mutex::new(Vec::new()));
    let mut ports = ReplayBindings::new();
    let loader = cache.clone();
    ports
        .add_cache("source", move |h, b| loader.load(h, b))
        .unwrap();
    ports
        .add_input("forward", 0, hydrate_f.bind(&source).unwrap())
        .unwrap();
    ports
        .add_input("observe", 0, hydrate_o.bind(&source).unwrap())
        .unwrap();
    ports
        .add_input(
            "consume",
            0,
            hydrate_c
                .bind_with_decoder(&forward, move |bytes| cache.decode_forwarded::<bool>(bytes))
                .unwrap(),
        )
        .unwrap();
    for (name, key) in [("a", a.output_key()), ("b", b.output_key())] {
        let indices = captured_indices.clone();
        ports
            .add_output(
                name,
                0,
                source
                    .configure_publisher(key, |p| {
                        p.observe(move |message| {
                            indices
                                .lock()
                                .unwrap()
                                .push((message.header.publisher_index, message.header.batch_index))
                        });
                        PortCapture::native(p, 2)
                    })
                    .unwrap(),
            )
            .unwrap();
    }
    ports
        .add_output(
            "forward",
            0,
            forward
                .configure_publisher(f.output_key(), |p| PortCapture::native(p, 4))
                .unwrap(),
        )
        .unwrap();
    ports
        .add_output(
            "observe",
            0,
            observed
                .configure_publisher(o.output_key(), |p| PortCapture::native(p, 1))
                .unwrap(),
        )
        .unwrap();
    ports
        .add_output(
            "consume",
            0,
            result
                .configure_publisher(c.output_key(), |p| PortCapture::native(p, 1))
                .unwrap(),
        )
        .unwrap();
    let mut graph = GraphBuilder::with_storage(&storage);
    graph.add_callback("consume", || Ok(Consume.bind(c, &forward, &result)?));
    graph.add_callback("observe", || Ok(Observe.bind(o, &source, &observed)?));
    graph.add_callback("forward", || Ok(Forward.bind(f, &source, &forward)?));
    graph.add_callback("b", || Ok(Source(20).bind(b, &source)?));
    graph.add_callback("a", || Ok(Source(10).bind(a, &source)?));
    let report = ExactReplayExecutor::new(graph.build().unwrap(), log, ports)
        .unwrap()
        .run()
        .unwrap();
    assert!(report.is_exact(), "{report:?}");
    assert_eq!(report.consumed_executions(), 10);
    assert_eq!(
        *captured_indices.lock().unwrap(),
        [
            (2, 0),
            (2, 1),
            (1, 0),
            (1, 1),
            (2, 0),
            (2, 1),
            (1, 0),
            (1, 1)
        ]
    );
}

#[test]
fn same_timestamp_batches_with_logged_sources_and_forwards() {
    replay_batches(true, true);
}

#[test]
fn same_timestamp_batches_with_logged_sources_and_reproduced_forwards() {
    replay_batches(true, false);
}

#[test]
fn same_timestamp_batches_with_reproduced_sources_and_logged_forwards() {
    replay_batches(false, true);
}

#[test]
fn same_timestamp_batches_with_reproduced_sources_and_forwards() {
    replay_batches(false, false);
}
