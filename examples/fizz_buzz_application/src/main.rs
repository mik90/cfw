use std::time::Duration;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use live_executor::LiveExecutor;
use test_tasks;

fn main() {
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))
        .expect("Could not register signal hook");

    let (nodes, _) = test_tasks::build_fizz_buzz_callback_nodes();
    let thread_count = 2;
    println!(
        "Building fizz buzz callback nodes with {} threads",
        thread_count
    );
    // TODO run logging build step
    let mut executor = LiveExecutor::new(thread_count, nodes);
    executor.start_threads();

    while !term.load(Ordering::Relaxed) {
        // Do some time-limited stuff here
        // (if this could block forever, then there's no guarantee the signal will have any
        // effect).
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("Recieved stop signal, stopping threads");

    executor.stop_threads().expect("Could not stop threads");
    println!("Done");
}
