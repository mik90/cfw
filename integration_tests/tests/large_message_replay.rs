use exact_replay_executor::{ExactReplayExecutor, ExactReplayPlan, ReplayLog};
use live_executor::LiveExecutor;
use logging::{
    CaptureOptions, ExecutionLogOptions, ExecutionLogPlan, GraphCapturePlan, LogSession,
    testing::InMemoryWriter,
};
use std::{
    io::Write,
    sync::{
        Arc, Mutex,
        mpsc::{self, TryRecvError},
    },
    time::{Duration, Instant},
};
use task::{
    LoanError, Output, RequiredInput,
    loggable::{DeserializeError, Loggable, SerializeError},
};
use task_graph::{ApplicationBuilder, CallbackSchedule, GraphPlan, PoolIndex};
use task_macros::task_callback;

mod common;

const LARGE_SIZE: usize = 100_000;
const MESSAGE_COUNT: usize = 2;
const LARGE_CHANNEL: &str = "large_message";
const SOURCE_PERIOD: Duration = Duration::from_millis(2);
const CAPTURE_CAPACITY: usize = 4;
const EXECUTION_LOG_CAPACITY: usize = 16;
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

type Checksums = Arc<Mutex<Vec<u64>>>;

struct LargeMessage {
    big_array: [u64; LARGE_SIZE],
}
impl Loggable for LargeMessage {
    type Context<'a>
        = ()
    where
        Self: 'a;

    fn serialize(&self, writer: &mut dyn Write) -> Result<(), SerializeError> {
        for value in &self.big_array {
            writer.write_all(&value.to_le_bytes())?;
        }
        Ok(())
    }

    #[expect(
        clippy::large_stack_frames,
        reason = "by-value deserialization of the large inline payload is unsupported"
    )]
    fn deserialize_with_ctx<'a>(_: &[u8], _: ()) -> Result<Self, DeserializeError>
    where
        Self: 'a,
    {
        // By-value deserialization would put the 800 KB payload on the stack.
        // Native replay hydrates through deserialize_into instead.
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "LargeMessage requires in-place deserialization",
        )
        .into())
    }

    fn deserialize_into<'a>(
        bytes: &[u8],
        slot: &mut std::mem::MaybeUninit<Self>,
        _: (),
    ) -> Result<(), DeserializeError>
    where
        Self: 'a,
    {
        const WORD_BYTES: usize = std::mem::size_of::<u64>();
        if bytes.len() != LARGE_SIZE * WORD_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "large message expected {} bytes, got {}",
                    LARGE_SIZE * WORD_BYTES,
                    bytes.len()
                ),
            )
            .into());
        }
        let ptr = slot.as_mut_ptr();
        for (index, chunk) in bytes.as_chunks::<8>().0.iter().enumerate() {
            let value = u64::from_le_bytes(*chunk);
            // SAFETY: `slot` points at an uninitialized LargeMessage and the
            // loop writes every element of big_array exactly once.
            unsafe {
                (&raw mut (*ptr).big_array)
                    .cast::<u64>()
                    .add(index)
                    .write(value);
            }
        }
        Ok(())
    }
}

fn expected_value(index: usize, sequence: u64) -> u64 {
    (index as u64)
        .wrapping_mul(3)
        .wrapping_add(1)
        .wrapping_add(sequence.wrapping_mul(1_000_000))
}

fn expected_checksum(sequence: u64) -> u64 {
    (0..LARGE_SIZE).fold(0u64, |checksum, index| {
        checksum.wrapping_add(expected_value(index, sequence))
    })
}

fn expected_checksums() -> Vec<u64> {
    (0..MESSAGE_COUNT as u64).map(expected_checksum).collect()
}

struct LargeMessagePublisher {
    next_sequence: u64,
    remaining: usize,
}
#[task_callback]
impl LargeMessagePublisher {
    fn run(&mut self, mut output: Output<LargeMessage>) -> Result<(), LoanError> {
        if self.remaining == 0 {
            return Ok(());
        }
        let slot = output.payload_uninit();
        let ptr = slot.as_mut_ptr();
        // SAFETY: the loan provides exclusive access to the allocation; every
        // element is initialized before the large message is assumed initialized.
        unsafe {
            let array_ptr = (&raw mut (*ptr).big_array).cast::<u64>();
            for index in 0..LARGE_SIZE {
                array_ptr
                    .add(index)
                    .write(expected_value(index, self.next_sequence));
            }
        }
        // SAFETY: all elements of big_array were initialized above.
        unsafe { output.assume_init() }.send();
        self.next_sequence += 1;
        self.remaining -= 1;
        Ok(())
    }
}

struct LargeMessageSink {
    checksums: Checksums,
    stop: Option<mpsc::Sender<()>>,
}
#[task_callback]
impl LargeMessageSink {
    fn run(
        &self,
        #[trigger(true)]
        #[capacity(2)]
        message: RequiredInput<LargeMessage>,
    ) {
        let sequence = message.big_array[0].wrapping_sub(1) / 1_000_000;
        assert!((sequence as usize) < MESSAGE_COUNT);
        let mut checksum = 0u64;
        for (index, value) in message.big_array.iter().enumerate() {
            assert_eq!(*value, expected_value(index, sequence), "index {index}");
            checksum = checksum.wrapping_add(*value);
        }
        let mut checksums = self.checksums.lock().unwrap();
        checksums.push(checksum);
        if checksums.len() == MESSAGE_COUNT
            && let Some(stop) = &self.stop
        {
            let _ = stop.send(());
        }
    }
}

fn build_plan(stop: Option<mpsc::Sender<()>>, checksums: Checksums) -> GraphPlan {
    let mut app = ApplicationBuilder::new();
    app.add_task(
        "large_source",
        LargeMessagePublisher {
            next_sequence: 0,
            remaining: MESSAGE_COUNT,
        },
        CallbackSchedule::periodic(SOURCE_PERIOD)
            .in_pool(PoolIndex::new(0))
            .with_execution_duration(Duration::from_millis(1)),
    )
    .output_channel("output", LARGE_CHANNEL);
    app.add_task(
        "large_sink",
        LargeMessageSink { checksums, stop },
        CallbackSchedule::input_triggered()
            .in_pool(PoolIndex::new(0))
            .with_execution_duration(Duration::from_millis(1)),
    )
    .input_channel("message", LARGE_CHANNEL);
    app.plan().unwrap()
}

fn run_live() -> (ReplayLog, Vec<u64>) {
    let checksums: Checksums = Default::default();
    let (stop_tx, stop_rx) = mpsc::channel();
    let mut plan = build_plan(Some(stop_tx), checksums.clone());
    let captures =
        GraphCapturePlan::declare(&mut plan, &CaptureOptions::new(CAPTURE_CAPACITY)).unwrap();
    let storage = plan.allocate().unwrap();
    let (graph, bindings) = storage.bind_and_build().unwrap();
    let execution_log =
        ExecutionLogPlan::for_graph(&graph, ExecutionLogOptions::new(EXECUTION_LOG_CAPACITY, 4))
            .unwrap();
    let log_storage = execution_log.allocate().unwrap();
    let (graph, metadata, execution_captures) = log_storage.attach(graph).unwrap();
    let mut captures = captures.bind(&bindings).unwrap();
    captures.extend(execution_captures);

    let writer = InMemoryWriter::new();
    let logged_data = writer.logged_data();
    let mut session = LogSession::new(writer, captures)
        .with_execution_log(metadata)
        .unwrap();

    let executor =
        LiveExecutor::new(task_graph::ThreadPoolConfig::try_from(1).unwrap(), graph).unwrap();
    let mut stopped_by_sink = false;
    executor
        .run_with(|stop| {
            let deadline = Instant::now() + STOP_TIMEOUT;
            loop {
                session.flush().unwrap();
                match stop_rx.try_recv() {
                    Ok(()) => {
                        stopped_by_sink = true;
                        stop.request_stop();
                        break;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        panic!("sink dropped without requesting a stop")
                    }
                }
                if stop.is_stopped() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for the sink to request a stop"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        })
        .unwrap();
    session.finish().unwrap();
    assert!(
        stopped_by_sink,
        "live executor stopped before the sink finished"
    );

    let live_checksums = checksums.lock().unwrap().clone();
    assert_eq!(live_checksums, expected_checksums());
    assert_eq!(
        logged_data
            .lock()
            .unwrap()
            .messages()
            .iter()
            .filter(|message| message.channel() == LARGE_CHANNEL)
            .count(),
        MESSAGE_COUNT
    );

    let log = common::replay_log_from_recorded_data(&logged_data);
    (log, live_checksums)
}

fn run_replay(log: ReplayLog) -> Vec<u64> {
    let checksums: Checksums = Default::default();
    let mut plan = build_plan(None, checksums.clone());
    let replay = ExactReplayPlan::declare(&mut plan, &log).unwrap();
    let storage = plan.allocate().unwrap();
    let bindings = storage.bind().unwrap();
    let replay_bindings = replay.bind(&bindings).unwrap();
    let graph = storage.build_with(&bindings).unwrap();
    let mut executor = ExactReplayExecutor::new(graph, log, replay_bindings).unwrap();
    let report = executor.run().unwrap();
    assert!(report.is_exact(), "{report:?}");
    assert_eq!(report.consumed_executions(), report.total_executions());
    let stats = &report.channel_stats()[LARGE_CHANNEL];
    assert!(stats.logged > 0, "expected logged large messages");
    assert_eq!(stats.gaps, 0, "unexpected gaps on '{LARGE_CHANNEL}'");
    assert_eq!(
        stats.mismatches, 0,
        "unexpected mismatches on '{LARGE_CHANNEL}'"
    );
    checksums.lock().unwrap().clone()
}

#[test]
#[cfg_attr(miri, ignore = "uses live worker threads and wall-clock scheduling")]
fn exact_replay_reproduces_large_inline_messages() {
    let (log, live_checksums) = run_live();
    let replayed_checksums = run_replay(log);
    assert_eq!(replayed_checksums, live_checksums);
    assert_eq!(live_checksums, expected_checksums());
}
