use clap::Parser;
use fizz_buzz_application::{BuildError, with_recording};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Parser, Debug)]
struct CliArgs {
    #[arg(short, long, default_value = "./tmp/log.ndjson")]
    log_path: PathBuf,
    /// Reproduce integers during exact replay instead of recording their payloads.
    #[arg(long)]
    no_log_integer: bool,
    /// Stop after this many collected strings; otherwise run until interrupted.
    #[arg(long)]
    count: Option<usize>,
    #[arg(long)]
    print: bool,
}

fn main() -> Result<(), BuildError> {
    let args = CliArgs::parse();
    if args.print {
        fizz_buzz_application::print_graph(false, false);
        return Ok(());
    }
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, term.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, term.clone())?;
    with_recording(
        &args.log_path,
        !args.no_log_integer,
        |graph, mut session, collected| {
            live_executor::LiveExecutor::new(2, graph)?.run_with(
                |stop| -> Result<(), BuildError> {
                    while !term.load(Ordering::Relaxed)
                        && !stop.is_stopped()
                        && !args.count.is_some_and(|count| collected.len() >= count)
                    {
                        session.flush()?;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(())
                },
            )??;
            session.finish()?;
            for value in collected.stored_strings() {
                println!("{value}");
            }
            Ok(())
        },
    )
}
