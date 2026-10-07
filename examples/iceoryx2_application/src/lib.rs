//! Two CFW graphs connected by an iceoryx2 integer channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use task::input::{InputSpan, OptionalInput, RequiredInput};
use task::iox2::{Iox2ChannelPlan, Iox2Event, Iox2OptionalInput, Iox2Output, Iox2Runtime};
use task::output::Output;
use task::{BuiltGraph, CallbackSchedule, ChannelPlan, GraphBuilder, GraphPlan};
use task_macros::task_callback;

const RESULT_CHANNEL: &str = "fizz_buzz_result";
const METRICS_CHANNEL: &str = "fizz_buzz_metrics";

pub const PUBLISH_PERIOD: Duration = Duration::from_millis(200);

pub struct Fizzer {
    next: u64,
    count: usize,
    published: Arc<AtomicUsize>,
    last_sent_at: Option<Instant>,
}

#[task_callback]
impl Fizzer {
    fn run(
        &mut self,
        acknowledgment: Iox2OptionalInput<u64>,
        _event: Iox2Event,
        mut integer: Iox2Output<u64>,
    ) {
        if self.published.load(Ordering::Relaxed) == self.count {
            return;
        }
        if self.next > 1 && acknowledgment.value().copied() != Some(self.next - 1) {
            return;
        }
        // Acknowledgments also wake this callback, so the periodic schedule
        // alone does not limit how quickly consecutive integers are sent.
        if self
            .last_sent_at
            .is_some_and(|last_sent_at| last_sent_at.elapsed() < PUBLISH_PERIOD)
        {
            return;
        }
        *integer = self.next;
        self.last_sent_at = Some(Instant::now());
        self.next += 1;
        integer.send();
        self.published.fetch_add(1, Ordering::Release);
    }
}

pub struct Calculator {
    last_value: u64,
}

#[task_callback]
impl Calculator {
    fn run(
        &mut self,
        integer: Iox2OptionalInput<u64>,
        _event: Iox2Event,
        mut result: Output<String>,
        mut acknowledgment: Iox2Output<u64>,
    ) {
        let Some(&value) = integer.value() else {
            return;
        };
        if value <= self.last_value {
            return;
        }
        self.last_value = value;
        *result = fizz_buzz(value);
        result.send();
        *acknowledgment = value;
        acknowledgment.send();
    }
}

pub fn fizz_buzz(value: u64) -> String {
    match (value.is_multiple_of(3), value.is_multiple_of(5)) {
        (true, true) => "FizzBuzz".into(),
        (true, false) => "Fizz".into(),
        (false, true) => "Buzz".into(),
        (false, false) => value.to_string(),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Metrics {
    pub fizz: usize,
    pub buzz: usize,
    pub fizz_buzz: usize,
    pub numbers: usize,
}

impl Metrics {
    pub fn total(self) -> usize {
        self.fizz + self.buzz + self.fizz_buzz + self.numbers
    }

    fn record(&mut self, result: &str) {
        match result {
            "FizzBuzz" => self.fizz_buzz += 1,
            "Fizz" => self.fizz += 1,
            "Buzz" => self.buzz += 1,
            _ => self.numbers += 1,
        }
    }
}

pub struct MetricsRecorder {
    metrics: Metrics,
}

#[task_callback]
impl MetricsRecorder {
    fn run(&mut self, mut results: InputSpan<String>, mut snapshot: Output<Metrics>) {
        let mut changed = false;
        for result in results.drain() {
            self.metrics.record(&result.message);
            changed = true;
        }
        if changed {
            *snapshot = self.metrics;
            snapshot.send();
        }
    }
}

pub struct MetricsSummary {
    count: usize,
    complete: Arc<AtomicBool>,
}

#[task_callback]
impl MetricsSummary {
    fn run(&self, metrics: RequiredInput<Metrics>) {
        if metrics.total() == self.count && !self.complete.load(Ordering::Relaxed) {
            println!(
                "Total: {} (fizz={}, buzz={}, fizzbuzz={}, numbers={})",
                metrics.total(),
                metrics.fizz,
                metrics.buzz,
                metrics.fizz_buzz,
                metrics.numbers
            );
            self.complete.store(true, Ordering::Release);
        }
    }
}

pub struct Printer {
    printed: Arc<AtomicUsize>,
}

#[task_callback]
impl Printer {
    fn run(&mut self, mut result: RequiredInput<String>, metrics: OptionalInput<Metrics>) {
        if let Some(metrics) = metrics.value() {
            println!(
                "{} (fizz={}, buzz={}, fizzbuzz={}, numbers={})",
                *result, metrics.fizz, metrics.buzz, metrics.fizz_buzz, metrics.numbers
            );
        } else {
            println!("{}", *result);
        }
        self.printed.fetch_add(1, Ordering::Release);
        result.clear();
    }
}

pub fn with_fizzer_graph<R>(
    channel: &str,
    count: usize,
    published: Arc<AtomicUsize>,
    run: impl for<'storage> FnOnce(BuiltGraph<'storage>) -> R,
) -> Result<R, Box<dyn std::error::Error>> {
    let acknowledgment = format!("{channel}_ack");
    let runtime = Iox2Runtime::new().map_err(|e| format!("{e:?}"))?;
    let mut integers = Iox2ChannelPlan::<u64>::new(channel, &runtime);
    let mut acknowledgments = Iox2ChannelPlan::<u64>::new(acknowledgment, &runtime);
    let declaration = FizzerDeclaration::from_keys(
        acknowledgments.subscriber(1),
        acknowledgments.events(1),
        integers.publisher(1),
    );
    let storage = GraphPlan::new((integers, acknowledgments))
        .allocate()
        .map_err(|e| format!("{e:?}"))?;
    let integers = storage.channels().0.build().map_err(|e| format!("{e:?}"))?;
    let acknowledgments = storage.channels().1.build().map_err(|e| format!("{e:?}"))?;
    let mut builder = GraphBuilder::with_storage(&storage);
    builder.add_scheduled_callback("fizzer", CallbackSchedule::periodic(PUBLISH_PERIOD), || {
        Ok(Fizzer {
            next: 1,
            count,
            published,
            last_sent_at: None,
        }
        .bind(declaration, &acknowledgments, &acknowledgments, &integers)?)
    });
    Ok(run(builder.build().map_err(|e| format!("{e:?}"))?))
}

pub fn with_buzzer_graph<R>(
    channel: &str,
    count: usize,
    complete: Arc<AtomicBool>,
    printed: Arc<AtomicUsize>,
    run: impl for<'storage> FnOnce(BuiltGraph<'storage>) -> R,
) -> Result<R, Box<dyn std::error::Error>> {
    let acknowledgment = format!("{channel}_ack");
    let runtime = Iox2Runtime::new().map_err(|e| format!("{e:?}"))?;
    let mut integers = Iox2ChannelPlan::<u64>::new(channel, &runtime);
    let mut acknowledgments = Iox2ChannelPlan::<u64>::new(acknowledgment, &runtime);
    let mut results = ChannelPlan::<String>::new(RESULT_CHANNEL);
    let mut metrics = ChannelPlan::<Metrics>::new(METRICS_CHANNEL);
    let calculator = CalculatorDeclaration::from_keys(
        integers.subscriber(1),
        integers.events(1),
        results.publisher(1),
        acknowledgments.publisher(1),
    );
    let recorder = MetricsRecorder::declare(&mut results, &mut metrics)?;
    let summary = MetricsSummary::declare(&mut metrics)?;
    let printer = Printer::declare(&mut results, &mut metrics)?;
    let storage = GraphPlan::new(((integers, acknowledgments), (results, metrics)))
        .allocate()
        .map_err(|e| format!("{e:?}"))?;
    let integers = storage
        .channels()
        .0
        .0
        .build()
        .map_err(|e| format!("{e:?}"))?;
    let acknowledgments = storage
        .channels()
        .0
        .1
        .build()
        .map_err(|e| format!("{e:?}"))?;
    let results = storage.channels().1.0.build();
    let metrics = storage.channels().1.1.build();
    let mut builder = GraphBuilder::with_storage(&storage);
    // Calculator has no periodic fallback: IPC notifications drive execution.
    builder.add_callback("calculator", || {
        Ok(Calculator { last_value: 0 }.bind(
            calculator,
            &integers,
            &integers,
            &results,
            &acknowledgments,
        )?)
    });
    builder.add_callback("metrics", || {
        Ok(MetricsRecorder {
            metrics: Metrics::default(),
        }
        .bind(recorder, &results, &metrics)?)
    });
    builder.add_callback("summary", || {
        Ok(MetricsSummary { count, complete }.bind(summary, &metrics)?)
    });
    builder.add_callback("printer", || {
        Ok(Printer { printed }.bind(printer, &results, &metrics)?)
    });
    Ok(run(builder.build().map_err(|e| format!("{e:?}"))?))
}
