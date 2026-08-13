//! How to plug in your own (de)serialization backend.
//!
//! The framework's serialization seam is the [`Loggable`] trait
//! (`task::loggable::Loggable`). Anything that implements it can be written to
//! and replayed from the framework's log file, no matter which serialization
//! library produces the bytes.
//!
//! Serde is the framework's built-in default: a blanket `impl` covers every
//! `Serialize + DeserializeOwned` type, and the framework's own log-file
//! *container* (file envelope, message headers, execution-log descriptor) is
//! serde-json. This example shows the other half of the story — logging a
//! payload type with a completely different backend, [facet], from the
//! application crate, with zero changes to the framework.
//!
//! What a custom backend needs from you:
//!
//! 1. Implement [`Loggable`] with `Context<'a> = ()`, `serialize` writing
//!    through your backend, and `deserialize_with_ctx` reading it back.
//! 2. Keep the type `Send + Sync` so automatic channel registration during
//!    graph build populates the serializer *and* deserializer + publisher
//!    factory (which is what makes replay work).
//! 3. Don't also derive serde on the type — the blanket impl would claim it
//!    and there's no way to override it.
//!
//! Why a macro instead of a blanket impl? A blanket `impl<T: Facet> Loggable
//! for T` is impossible: the orphan rule forbids it in application crates, and
//! it would overlap the serde blanket (a type could implement both) even inside
//! the framework. So the backend is opted into per type — and this local
//! `macro_rules!` is all the ceremony it takes.
//!
//! Run `live` to write a facet-serialized log, then `verify` to read it back
//! and prove the round-trip.

use clap::{Parser, Subcommand};
use facet::Facet;
use live_executor::LiveExecutor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use task::execution_log::ExecutionLogLevel;
use task::loggable::Loggable;
use task::{CallbackBuilder, Output, task_graph_builder};
use task_macros::task_callback;

// ───────────────────────────── CLI ────────────────────────────────────────

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
    /// Run the producer live and log facet-serialized payloads.
    Live {
        /// File to log to including file extension.
        #[arg(short, long, default_value = "./tmp/log.ndjson")]
        log_path: PathBuf,
    },
    /// Read a recorded log back and deserialize the facet payloads.
    Verify {
        /// Path to the log to read back, including file extension.
        #[arg(short, long, default_value = "./tmp/log.ndjson")]
        log_path: PathBuf,
    },
}

// ─────────────────────── Payload + backend ────────────────────────────────

/// The message payload. `#[derive(Facet)]` gives facet-json the shape
/// information it needs to (de)serialize the struct — no serde anywhere.
#[derive(Facet, Debug, PartialEq, Default)]
struct MyCustomData {
    integer: u64,
    string: String,
}

/// Generate a [`Loggable`] impl backed by facet-json for `$t`.
///
/// This is the entire "backend plug-in": swap the two calls for your
/// framework of choice and the type becomes loggable. `$t` must be
/// `Send + Sync` (for replay registration) and must not derive serde.
macro_rules! facet_loggable {
    ($t:ty) => {
        impl task::loggable::Loggable for $t {
            type Context<'a> = ();

            fn serialize(
                &self,
                w: &mut dyn std::io::Write,
            ) -> Result<(), task::loggable::SerializeError> {
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
    };
}

facet_loggable!(MyCustomData);

// ─────────────────────────── Task wiring ──────────────────────────────────

/// Publishes `MyCustomData` on the `custom_data` channel every 100ms.
struct MyTask {}

#[task_callback]
impl MyTask {
    fn run(
        &self,
        context: &task::Context,
        #[channel("custom_data")] mut output: Output<MyCustomData>,
    ) {
        output.integer = context.now.to_nanoseconds() as u64;
        output.string = output.integer.to_string();
        output.send();
    }

    fn callback_builder(self) -> CallbackBuilder {
        self.builder()
            .with_name("CustomTask")
            .with_periodic_execution(Duration::from_millis(100))
            .with_execution_duration_callback(|| Duration::from_micros(100))
    }
}

// ───────────────────────────── Runners ────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = CliArgs::parse();
    match args.command {
        Subcommands::Live { log_path } => run_live(log_path, args.print),
        Subcommands::Verify { log_path } => verify(&log_path),
    }
}

/// Build the graph (with the logging build step) and run it until a stop
/// signal arrives, writing facet-serialized `custom_data` messages to `log_path`.
fn run_live(log_path: PathBuf, print: bool) -> Result<(), Box<dyn std::error::Error>> {
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))?;

    println!("Building the facet-serialized task graph for live execution");

    let logging_build_step = Box::new(logging::log_build_step::LoggingBuildStep::new(
        logging::LogTaskConfiguration {
            output_path: log_path,
            period: Duration::from_millis(1000),
            num_tasks: 1,
        },
    ));

    let mut graph = task_graph_builder::TaskGraphBuilder::new()
        .add_pool(1, |p| p.add_callback_builder(MyTask {}.callback_builder()))
        .with_execution_log_level(ExecutionLogLevel::Whole)
        .add_build_step(logging_build_step)
        .build()
        .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;

    if print {
        graph.print();
        return Ok(());
    }

    let mut executor = LiveExecutor::new_multi_pool_with_execution_log(
        std::mem::take(&mut graph.pools),
        std::mem::take(&mut graph.execution_log_publishers),
        Duration::from_millis(500),
    );
    executor.start_threads();

    while !term.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("Received stop signal, stopping threads");

    executor
        .stop_threads()
        .map_err(|e| -> Box<dyn std::error::Error> { format!("{e:?}").into() })?;
    println!("Done");
    Ok(())
}

/// Read a recorded log back and deserialize every `custom_data` payload with
/// facet, printing the reconstructed values.
fn verify(log_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use logging::log_file::LogFileReader;

    let file = std::fs::File::open(log_path)?;
    let reader =
        logging::log_file_json::JsonLogFileReader::from_reader(std::io::BufReader::new(file))?;

    let mut count = 0;
    for entry in reader.iter() {
        if entry.channel_name != "custom_data" {
            continue;
        }
        let value = MyCustomData::deserialize(entry.serialized_body)
            .map_err(|e| format!("failed to facet-deserialize custom_data entry: {e}"))?;
        println!("{value:?}");
        count += 1;
    }

    println!("verified {count} custom_data messages round-tripped through facet");
    Ok(())
}

// ───────────────────────────── Tests ──────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use logging::log_file::{LogFileReader, LogFileWriter};

    /// Prove the seam end to end without running a live executor: write a
    /// facet-serialized payload through the framework's JSONL writer, read it
    /// back, and deserialize it with facet. Miri can't add/remove files.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn facet_payload_round_trips_through_log_file() {
        let path = std::env::temp_dir().join(format!(
            "cfw_facet_roundtrip_{}_{}.ndjson",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = std::fs::remove_file(&path);

        let payload = MyCustomData {
            integer: 42,
            string: "hello facet".to_owned(),
        };

        {
            let file = std::fs::File::create(&path).expect("create log");
            let mut writer =
                logging::log_file_json::JsonLogFileWriter::new(std::io::BufWriter::new(file));
            let mut bytes = Vec::new();
            Loggable::serialize(&payload, &mut bytes).expect("facet serialize");
            writer
                .store_message(
                    "custom_data",
                    &task::message::MessageHeader::default(),
                    &bytes,
                )
                .expect("store message");
            writer.flush().expect("flush");
        }

        let file = std::fs::File::open(&path).expect("open log");
        let reader =
            logging::log_file_json::JsonLogFileReader::from_reader(std::io::BufReader::new(file))
                .expect("parse log");

        let entries: Vec<_> = reader.iter().collect();
        assert_eq!(entries.len(), 1, "one logged message expected");
        assert_eq!(entries[0].channel_name, "custom_data");

        let decoded =
            MyCustomData::deserialize(entries[0].serialized_body).expect("facet deserialize");
        assert_eq!(
            decoded, payload,
            "facet round-trip must preserve the payload"
        );

        let _ = std::fs::remove_file(&path);
    }
}
