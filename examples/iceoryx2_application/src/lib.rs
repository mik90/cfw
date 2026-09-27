//! Two CFW graphs connected by an iceoryx2 integer channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use task::callback_builder::CallbackBuilder;
use task::input::{InputSpan, OptionalInput, RequiredInput};
use task::iox2::{Iox2Event, Iox2OptionalInput, Iox2Output};
use task::output::Output;
use task::task_graph_builder::{BuiltTaskGraph, TaskGraphBuildError, TaskGraphBuilder};
use task_macros::task_callback;

const RESULT_CHANNEL: &str = "fizz_buzz_result";
const METRICS_CHANNEL: &str = "fizz_buzz_metrics";

pub const PUBLISH_PERIOD: Duration = Duration::from_millis(200);
const METRICS_PERIOD: Duration = Duration::from_millis(100);

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

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_periodic_execution(PUBLISH_PERIOD)
            .with_execution_duration_callback(|| Duration::ZERO)
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

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_periodic_execution(Duration::from_millis(50))
            .with_execution_duration_callback(|| Duration::ZERO)
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
        for result in results.drain_inputs() {
            self.metrics.record(&result.message);
            changed = true;
        }
        if changed {
            *snapshot = self.metrics;
            snapshot.send();
        }
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_periodic_execution(METRICS_PERIOD)
            .with_execution_duration_callback(|| Duration::ZERO)
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

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_execution_duration_callback(|| Duration::ZERO)
    }
}

pub struct Printer {
    printed: Arc<AtomicUsize>,
}

#[task_callback]
impl Printer {
    fn run(&mut self, result: RequiredInput<String>, metrics: OptionalInput<Metrics>) {
        if let Some(metrics) = metrics.value() {
            println!(
                "{} (fizz={}, buzz={}, fizzbuzz={}, numbers={})",
                *result, metrics.fizz, metrics.buzz, metrics.fizz_buzz, metrics.numbers
            );
        } else {
            println!("{}", *result);
        }
        self.printed.fetch_add(1, Ordering::Release);
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_execution_duration_callback(|| Duration::ZERO)
    }
}

pub fn fizzer_graph(
    channel: &str,
    count: usize,
    published: Arc<AtomicUsize>,
) -> Result<BuiltTaskGraph, TaskGraphBuildError> {
    let acknowledgment = format!("{channel}_ack");
    TaskGraphBuilder::new()
        .add_pool(1, |pool| {
            pool.add_callback_builder(
                Fizzer {
                    next: 1,
                    count,
                    published,
                    last_sent_at: None,
                }
                .callback_builder()
                .with_subscriber_channels(&[&acknowledgment, &acknowledgment])
                .with_publisher_channels(&[channel]),
            )
        })
        .build()
}

pub fn buzzer_graph(
    channel: &str,
    count: usize,
    complete: Arc<AtomicBool>,
    printed: Arc<AtomicUsize>,
) -> Result<BuiltTaskGraph, TaskGraphBuildError> {
    let acknowledgment = format!("{channel}_ack");
    TaskGraphBuilder::new()
        .add_pool(1, |pool| {
            pool.add_callback_builder(
                Calculator { last_value: 0 }
                    .callback_builder()
                    .with_subscriber_channels(&[channel, channel])
                    .with_publisher_channels(&[RESULT_CHANNEL, &acknowledgment]),
            )
            .add_callback_builder(
                MetricsRecorder {
                    metrics: Metrics::default(),
                }
                .callback_builder()
                .with_subscriber_channels(&[RESULT_CHANNEL])
                .with_publisher_channels(&[METRICS_CHANNEL]),
            )
            .add_callback_builder(
                MetricsSummary { count, complete }
                    .callback_builder()
                    .with_subscriber_channels(&[METRICS_CHANNEL]),
            )
            .add_callback_builder(
                Printer { printed }
                    .callback_builder()
                    .with_subscriber_channels(&[RESULT_CHANNEL, METRICS_CHANNEL]),
            )
        })
        .build()
}
