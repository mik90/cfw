use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use iceoryx2_application::{PUBLISH_PERIOD, with_buzzer_graph, with_fizzer_graph};
use live_executor::LiveExecutor;

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

fn run_fizzer(count: usize, channel: &str) -> Result<(), Box<dyn std::error::Error>> {
    let published = Arc::new(AtomicUsize::new(0));
    with_fizzer_graph(
        channel,
        count,
        published,
        |graph| -> Result<(), Box<dyn std::error::Error>> {
            let executor = LiveExecutor::new(1, graph)?;
            // The launcher closes stdin after the buzzer consumes the final sample.
            executor.run_with(|_| io::copy(&mut io::stdin(), &mut io::sink()))??;
            Ok(())
        },
    )??;
    Ok(())
}

fn run_buzzer(count: usize, channel: &str) -> Result<(), Box<dyn std::error::Error>> {
    let complete = Arc::new(AtomicBool::new(false));
    let printed = Arc::new(AtomicUsize::new(0));
    with_buzzer_graph(
        channel,
        count,
        Arc::clone(&complete),
        Arc::clone(&printed),
        |graph| -> Result<(), Box<dyn std::error::Error>> {
            let executor = LiveExecutor::new(1, graph)?;
            executor.run_with(|stop| -> Result<(), Box<dyn std::error::Error>> {
                // run_with begins only after the readiness waitset has attached its listeners.
                println!("{READY}");
                io::stdout().flush()?;
                let deadline = Instant::now()
                    + PUBLISH_PERIOD.saturating_mul(u32::try_from(count).unwrap_or(u32::MAX))
                    + Duration::from_secs(10);
                loop {
                    if printed.load(Ordering::Acquire) >= count && complete.load(Ordering::Acquire)
                    {
                        break;
                    }
                    if stop.is_stopped() {
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

                Ok(())
            })??;
            Ok(())
        },
    )??;
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
