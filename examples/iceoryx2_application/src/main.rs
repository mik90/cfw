use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use iceoryx2_application::{PUBLISH_PERIOD, buzzer_graph, fizzer_graph};
use live_executor::LiveExecutor;
use task::executor::ExecutorParams;

const READY: &str = "IOX2_BUZZER_READY";

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Role {
    Launcher,
    Fizzer,
    Buzzer,
}

#[derive(Parser)]
#[command(about = "Run FizzBuzz in two CFW processes connected by iceoryx2")]
struct Args {
    /// Number of integers to publish (starting at 1).
    #[arg(long, default_value_t = 30)]
    count: usize,

    #[arg(long, value_enum, default_value = "launcher", hide = true)]
    role: Role,

    #[arg(long, hide = true)]
    channel: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.count == 0 {
        return Err("--count must be positive".into());
    }
    match args.role {
        Role::Launcher => launch(args.count),
        Role::Fizzer => run_fizzer(
            args.count,
            args.channel.as_deref().ok_or("missing channel")?,
        ),
        Role::Buzzer => run_buzzer(
            args.count,
            args.channel.as_deref().ok_or("missing channel")?,
        ),
    }
}

fn executor_params(graph: &mut task::task_graph_builder::BuiltTaskGraph) -> ExecutorParams {
    ExecutorParams::new(std::mem::take(&mut graph.pools))
        .with_iox2_context(graph.iox2_context.take())
}

fn run_fizzer(count: usize, channel: &str) -> Result<(), Box<dyn std::error::Error>> {
    let published = Arc::new(AtomicUsize::new(0));
    let mut graph =
        fizzer_graph(channel, count, Arc::clone(&published)).map_err(|error| error.to_string())?;
    let mut executor = LiveExecutor::new_multi_pool(executor_params(&mut graph));
    executor.try_start_threads()?;

    // Keep the publisher's iceoryx2 node and service alive until the buzzer
    // has consumed the final sample. The launcher closes stdin at that point.
    io::copy(&mut io::stdin(), &mut io::sink())?;
    executor
        .stop_threads()
        .map_err(|errors| format!("could not stop fizzer workers: {errors:?}"))?;
    Ok(())
}

fn run_buzzer(count: usize, channel: &str) -> Result<(), Box<dyn std::error::Error>> {
    let complete = Arc::new(AtomicBool::new(false));
    let printed = Arc::new(AtomicUsize::new(0));
    let mut graph = buzzer_graph(channel, count, Arc::clone(&complete), Arc::clone(&printed))
        .map_err(|error| error.to_string())?;
    let mut executor = LiveExecutor::new_multi_pool(executor_params(&mut graph));
    executor.try_start_threads()?;
    println!("{READY}");
    io::stdout().flush()?;

    let deadline = Instant::now()
        + PUBLISH_PERIOD.saturating_mul(u32::try_from(count).unwrap_or(u32::MAX))
        + Duration::from_secs(10);
    loop {
        if printed.load(Ordering::Acquire) >= count && complete.load(Ordering::Acquire) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out: printed {} of {count} results (final metrics: {})",
                printed.load(Ordering::Acquire),
                complete.load(Ordering::Acquire)
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    executor
        .stop_threads()
        .map_err(|errors| format!("could not stop buzzer workers: {errors:?}"))?;
    Ok(())
}

fn launch(count: usize) -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let channel = format!("cfw_fizz_buzz_{}", std::process::id());
    let mut buzzer = Command::new(&executable)
        .args([
            "--role",
            "buzzer",
            "--count",
            &count.to_string(),
            "--channel",
            &channel,
        ])
        .stdout(Stdio::piped())
        .spawn()?;
    let mut output = BufReader::new(buzzer.stdout.take().expect("piped buzzer stdout"));
    let mut line = String::new();
    loop {
        line.clear();
        if output.read_line(&mut line)? == 0 {
            return Err(format!("buzzer exited before becoming ready: {}", buzzer.wait()?).into());
        }
        if line.trim_end() == READY {
            break;
        }
        print!("{line}");
    }

    let mut fizzer = match Command::new(executable)
        .args([
            "--role",
            "fizzer",
            "--count",
            &count.to_string(),
            "--channel",
            &channel,
        ])
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            buzzer.kill()?;
            buzzer.wait()?;
            return Err(error.into());
        }
    };

    let relay = std::thread::spawn(move || -> io::Result<()> {
        for line in output.lines() {
            println!("{}", line?);
        }
        Ok(())
    });
    let buzzer_status = buzzer.wait()?;
    relay.join().expect("buzzer output relay panicked")?;
    // Closing the pipe tells the fizzer it can release its iceoryx2 node.
    drop(fizzer.stdin.take());
    let fizzer_status = fizzer.wait()?;
    if !buzzer_status.success() || !fizzer_status.success() {
        return Err(format!("buzzer: {buzzer_status}; fizzer: {fizzer_status}").into());
    }
    Ok(())
}
