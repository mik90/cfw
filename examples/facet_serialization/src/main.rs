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
//! 2. Declare a typed capture before allocating graph storage; bind it to a
//!    log session. A typed replay source uses the same codec when replaying.
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
use logging::{CapturePlan, ExecutionRecorder, LogSession};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use task::loggable::Loggable;
use task::{CallbackSchedule, ChannelPlan, GraphBuilder, GraphPlan, Output};
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

            fn deserialize_with_ctx<'a>(
                bytes: &[u8],
                _ctx: Self::Context<'a>,
            ) -> Result<Self, task::loggable::DeserializeError>
            where
                Self: 'a,
            {
                facet_json::from_slice(bytes).map_err(Into::into)
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
}

// ───────────────────────────── Runners ────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = CliArgs::parse();
    match args.command {
        Subcommands::Live { log_path } => run_live(log_path, args.print),
        Subcommands::Verify { log_path } => verify(&log_path),
    }
}

/// Build the graph and bound captures, then run until a stop
/// signal arrives, writing facet-serialized `custom_data` messages to `log_path`.
fn run_live(
    log_path: PathBuf,
    print: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))?;

    println!("Building the facet-serialized task graph for live execution");

    if print {
        println!("CustomTask -> custom_data (100ms period)");
        return Ok(());
    }
    let mut channel = ChannelPlan::new("custom_data");
    let declaration = MyTask::declare(&mut channel)?;
    let capture = CapturePlan::declare(&mut channel, 32);
    let storage = GraphPlan::new(channel)
        .allocate()
        .map_err(|e| format!("{e:?}"))?;
    let bindings = storage.channels().build();
    let mut builder = GraphBuilder::with_storage(&storage);
    builder.add_scheduled_callback(
        "CustomTask",
        CallbackSchedule::periodic(Duration::from_millis(100))
            .with_execution_duration(Duration::from_micros(100)),
        || Ok(MyTask {}.bind(declaration, &bindings)?),
    );
    let recorder = ExecutionRecorder::new(32);
    let graph = recorder.attach(builder.build().map_err(|e| format!("{e:?}"))?)?;
    let writer = logging::log_file_json::JsonLogFileWriter::new(std::io::BufWriter::new(
        std::fs::File::create(log_path)?,
    ));
    let mut session =
        LogSession::new(writer, vec![capture.bind(&bindings)?]).with_recording(recorder)?;
    LiveExecutor::new(1, graph)?.run_with(|stop| -> Result<(), logging::BoxedLogError> {
        while !term.load(Ordering::Relaxed) && !stop.is_stopped() {
            session.flush()?;
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    })??;
    session.finish()?;
    println!("Done");
    Ok(())
}

/// Read a recorded log back and deserialize every `custom_data` payload with
/// facet, printing the reconstructed values.
fn verify(log_path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    #[cfg_attr(miri, ignore = "Miri doesn't support file I/O")]
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
