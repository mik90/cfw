use clap::{Parser, Subcommand};
use facet::Facet;
use live_executor::LiveExecutor;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use task::loggable::Loggable;
use task::{CallbackBuilder, Output, task_graph_builder};
use task_macros::task_callback;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CliArgs {
    /// Print the task graph and exit without running.
    #[arg(long)]
    print: bool,

    #[command(subcommand)]
    command: Subcommands,
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    /// Executes code based off wall-clock and outputs log
    Live {
        /// File to log to including file extension.
        #[arg(short, long, default_value = "./tmp/log.ndjson")]
        log_path: PathBuf,
    },
    /// Replays code from input log based off fixed replay speed, writes new log out
    LiveReplay {
        /// Path to the input log including file extension.
        #[arg(short, long, default_value = "./tmp/log.ndjson")]
        input_log_path: PathBuf,
        /// Path to log to including file extension.
        #[arg(short, long, default_value = "./tmp/replay-log.ndjson")]
        output_log_path: PathBuf,
    },
}

#[derive(Facet, Debug, PartialEq, Default)]
struct MyCustomData {
    integer: u64,
    string: String,
}

struct MyTask {}

#[task_callback]
impl MyTask {
    fn run(&self, context: &task::Context, mut output: Output<MyCustomData>) {
        output.integer = context.now.to_nanoseconds() as u64;
        output.string = output.integer.to_string();
        output.send();
    }

    fn callback_builder() -> CallbackBuilder {
        CallbackBuilder::new("CustomTask".to_owned(), Box::new(MyTask {}.build()))
            .with_publisher_channels(&["custom_data"])
            .with_next_execution_time_callback(|t| Some(t + Duration::from_millis(100)))
            .with_execution_duration_callback(|| Duration::from_micros(100))
    }
}

/// Serialization backend lives outside the framework: `Loggable` is backend
/// agnostic, so facet can stand in for serde without the framework knowing.
impl Loggable for MyCustomData {
    type Context<'a> = ();

    fn serialize(&self, w: &mut dyn std::io::Write) -> Result<(), task::loggable::SerializeError> {
        facet_json::to_writer_std(w, self)?;
        Ok(())
    }

    fn deserialize_with_ctx(
        bytes: &[u8],
        _ctx: Self::Context<'_>,
    ) -> Result<Self, task::loggable::DeserializeError> {
        facet_json::from_slice(bytes)
            .map_err(|e| task::loggable::DeserializeError::Other(Box::new(e)))
    }
}

fn main() {
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))
        .expect("Could not register signal hook");
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))
        .expect("Could not register signal hook");

    let args = CliArgs::parse();

    println!("Building fizz buzz callback nodes for live execution");

    let output_log_path = match args.command {
        Subcommands::Live { log_path } => log_path,
        Subcommands::LiveReplay {
            output_log_path, ..
        } => output_log_path,
    };

    let logging_build_step = Box::new(logging::log_build_step::LoggingBuildStep::new(
        logging::LogTaskConfiguration {
            output_path: output_log_path,
            period: std::time::Duration::from_millis(1000),
            num_tasks: 1,
        },
    ));

    let graph = task_graph_builder::TaskGraphBuilder::new()
        .add_pool(1, |p| p.add_callback_builder(MyTask::callback_builder()))
        .with_log_executions(true)
        .add_build_step(logging_build_step)
        .build()
        .expect("Could not build task");

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

    while !term.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("Received stop signal, stopping threads");

    executor.stop_threads().expect("Could not stop threads");
    println!("Done");
}
