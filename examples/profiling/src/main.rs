use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use arrayvec::ArrayString;
use clap::Parser;
use live_executor::LiveExecutor;
use signal_hook::consts::{SIGINT, SIGTERM};
use task::callback::CallbackNode;
use task::callback_builder::CallbackBuilder;
use task::execution_log::ExecutionLogLevel;
use task::input::RequiredInput;
use task::output::Output;
use task::task_graph_builder::TaskGraphBuilder;
use task_macros::task_callback;

const FIZZ_BUZZ_STRING_CHANNEL: &str = "fizz_buzz_string";

type FizzBuzzString = ArrayString<32>;

struct IncrementingIntegerPublisher {
    value: u64,
    period: Duration,
}

#[task_callback]
impl IncrementingIntegerPublisher {
    fn run(&mut self, mut output: Output<u64>) {
        *output = self.value;
        self.value = self.value.wrapping_add(1);
        output.send();
    }

    fn callback_builder(self) -> CallbackBuilder {
        let period = self.period;
        self.builder()
            .with_periodic_execution(period)
            .with_execution_duration_callback(|| Duration::ZERO)
    }
}

struct FizzBuzzCalculator;

#[task_callback]
impl FizzBuzzCalculator {
    fn run(&mut self, integer: RequiredInput<u64>, mut fizz_buzz_string: Output<FizzBuzzString>) {
        let n = *integer;
        let is_fizz = n.is_multiple_of(3);
        let is_buzz = n.is_multiple_of(5);

        if is_fizz && is_buzz {
            *fizz_buzz_string = FizzBuzzString::from("FizzBuzz").expect("FizzBuzz fits");
        } else if is_fizz {
            *fizz_buzz_string = FizzBuzzString::from("Fizz").expect("Fizz fits");
        } else if is_buzz {
            *fizz_buzz_string = FizzBuzzString::from("Buzz").expect("Buzz fits");
        } else {
            write_u64_truncated(&mut fizz_buzz_string, n);
        }
        fizz_buzz_string.send();
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_execution_duration_callback(|| Duration::ZERO)
    }
}

fn decimal_digits(n: u64) -> u32 {
    n.checked_ilog10().unwrap_or(0) + 1
}

fn write_u64_truncated(buf: &mut FizzBuzzString, n: u64) {
    if write!(buf, "{n}").is_ok() {
        return;
    }
    let keep = buf.capacity().saturating_sub("...".len()).max(1);
    let mut remaining = n;
    while decimal_digits(remaining) > keep as u32 {
        remaining /= 10;
    }
    buf.clear();
    write!(buf, "{remaining}").expect("truncated number fits");
    buf.push_str("...");
}

struct StringCollector {
    counter: Arc<AtomicU64>,
}

#[task_callback]
impl StringCollector {
    fn run(&self, string: RequiredInput<FizzBuzzString>) {
        self.counter
            .fetch_add(string.len() as u64, Ordering::Relaxed);
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_execution_duration_callback(|| Duration::ZERO)
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CliArgs {
    /// Publisher execution period in microseconds. Lower is more stressful.
    #[arg(long, default_value_t = 100)]
    period_us: u64,

    /// Number of independent integer-publisher -> fizz-buzz chains, all
    /// feeding a single string collector. More chains means more scheduling.
    #[arg(long, default_value_t = 1)]
    sets: usize,

    /// Number of worker threads in the pool.
    #[arg(long, default_value_t = 2)]
    threads: usize,

    /// Auto-stop the executor after this many seconds.
    #[arg(long, default_value_t = 15)]
    duration_secs: u64,

    /// Print the task graph and exit without running.
    #[arg(long)]
    print: bool,
}

fn main() {
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&term))
        .expect("Could not register signal hook");
    signal_hook::flag::register(SIGINT, Arc::clone(&term)).expect("Could not register signal hook");

    let args = CliArgs::parse();
    let counter = Arc::new(AtomicU64::new(0));
    let period = Duration::from_micros(args.period_us);

    let mut callbacks: Vec<CallbackNode> = Vec::new();
    for set in 0..args.sets {
        let integer_channel = format!("integer_{set}");
        callbacks.push(
            IncrementingIntegerPublisher { value: 0, period }
                .callback_builder()
                .with_name(format!("IncrementingIntegerPublisher({integer_channel})"))
                .with_publisher_channels(&[integer_channel.as_str()])
                .build()
                .expect("build publisher"),
        );
        callbacks.push(
            FizzBuzzCalculator
                .callback_builder()
                .with_name(format!("FizzBuzzCalculator({integer_channel})"))
                .with_subscriber_channels(&[integer_channel.as_str()])
                .with_publisher_channels(&[FIZZ_BUZZ_STRING_CHANNEL])
                .build()
                .expect("build calculator"),
        );
    }
    callbacks.push(
        StringCollector {
            counter: counter.clone(),
        }
        .callback_builder()
        .with_subscriber_channels(&[FIZZ_BUZZ_STRING_CHANNEL])
        .build()
        .expect("build collector"),
    );
    let graph = TaskGraphBuilder::new()
        .add_pool(args.threads, move |p| {
            callbacks.into_iter().fold(p, |p, cb| p.add_callback(cb))
        })
        .with_execution_log_level(ExecutionLogLevel::Whole)
        .build()
        .expect("Could not build profiling task graph");

    if args.print {
        graph.print();
        return;
    }

    let mut executor = LiveExecutor::new_multi_pool_with_execution_log(
        graph.pools,
        graph.execution_log_publishers,
        Duration::from_millis(500),
    );
    executor.start_threads();

    let deadline = std::time::Instant::now() + Duration::from_secs(args.duration_secs);
    let timed_out = loop {
        if term.load(Ordering::Relaxed) {
            break false;
        }
        if std::time::Instant::now() >= deadline {
            break true;
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    if timed_out {
        println!("Auto-stopped after {} seconds", args.duration_secs);
    } else {
        println!("Received stop signal, stopping threads");
    }

    executor.stop_threads().expect("Could not stop threads");
    println!(
        "Collected {} fizz-buzz strings",
        counter.load(Ordering::Relaxed)
    );
}
