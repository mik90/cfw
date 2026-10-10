use clap::Parser;
use fizz_buzz_application::{BuildError, with_replay_graph};
use live_replay_executor::{LiveReplayConfig, LiveReplayExecutor};
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
    #[arg(short, long)]
    log_path: PathBuf,
    #[arg(short, long, default_value_t = 1.0)]
    speed: f64,
    /// Replay recorded strings directly (also supports logs with unlogged integers).
    #[arg(long)]
    strings_only: bool,
    #[arg(long)]
    print: bool,
}

fn main() -> Result<(), BuildError> {
    let args = CliArgs::parse();
    if args.print {
        fizz_buzz_application::print_graph(args.strings_only, true);
        return Ok(());
    }
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, term.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, term.clone())?;
    with_replay_graph(args.strings_only, |graph, sources, collected| {
        let reader = logging::SortedLogStreamReader::from_path(&args.log_path, 1024)?;
        let selected = if args.strings_only {
            test_tasks::FIZZ_BUZZ_STRING_CHANNEL
        } else {
            test_tasks::INTEGER_CHANNEL
        };
        if !reader
            .channel_names()
            .iter()
            .any(|channel| channel == selected)
        {
            return Err(format!(
                "log has no '{selected}' payloads; select a recorded source channel"
            )
            .into());
        }
        let executor = LiveReplayExecutor::new(
            graph,
            reader,
            sources,
            LiveReplayConfig {
                speed: args.speed,
                denylist: fizz_buzz_application::replay_denylist(args.strings_only),
                ..Default::default()
            },
        )?;
        let (_, completion) = executor.run_with(|control| {
            while !control.is_stopped() && !term.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
        })?;
        for value in collected.stored_strings() {
            println!("{value}");
        }
        println!("{completion:?}");
        Ok(())
    })
}
