use std::sync::{Arc, Barrier};
use std::thread;

use base::mpsc_queue::MpscQueue2;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use crossbeam_queue::ArrayQueue;

const SMALL_CAP: usize = 65536;
const LARGE_CAP: usize = 4 * 65536;

/// N producers each push `per_producer` items, 1 consumer drains them.
/// Returns after all items are pushed and popped.
fn run_workload(queue: &Arc<MpscQueue2<u64>>, n_producers: usize, per_producer: usize) {
    let total = n_producers * per_producer;
    let start = Arc::new(Barrier::new(n_producers + 1));
    let done = Arc::new(Barrier::new(n_producers + 1));

    let mut handles = Vec::new();
    for thread_id in 0..n_producers {
        let queue = Arc::clone(queue);
        let start = Arc::clone(&start);
        let done = Arc::clone(&done);
        handles.push(thread::spawn(move || {
            start.wait();
            for i in 0..per_producer {
                let value = (thread_id as u64) * (per_producer as u64) + i as u64;
                while !queue.push(value) {
                    std::hint::spin_loop();
                }
            }
            done.wait();
        }));
    }

    start.wait();
    let mut count = 0;
    while count < total {
        if queue.pop().is_some() {
            count += 1;
            black_box(count);
        } else {
            std::hint::spin_loop();
        }
    }
    done.wait();

    for handle in handles {
        handle.join().unwrap();
    }
}

/// Same workload, using crossbeam's ArrayQueue (MPMC).
fn run_workload_crossbeam(queue: &Arc<ArrayQueue<u64>>, n_producers: usize, per_producer: usize) {
    let total = n_producers * per_producer;
    let start = Arc::new(Barrier::new(n_producers + 1));
    let done = Arc::new(Barrier::new(n_producers + 1));

    let mut handles = Vec::new();
    for thread_id in 0..n_producers {
        let queue = Arc::clone(queue);
        let start = Arc::clone(&start);
        let done = Arc::clone(&done);
        handles.push(thread::spawn(move || {
            start.wait();
            for i in 0..per_producer {
                let value = (thread_id as u64) * (per_producer as u64) + i as u64;
                while queue.push(value).is_err() {
                    std::hint::spin_loop();
                }
            }
            done.wait();
        }));
    }

    start.wait();
    let mut count = 0;
    while count < total {
        if queue.pop().is_some() {
            count += 1;
            black_box(count);
        } else {
            std::hint::spin_loop();
        }
    }
    done.wait();

    for handle in handles {
        handle.join().unwrap();
    }
}

fn bench_throughput(c: &mut Criterion) {
    let items = 1_000_000;
    let thread_counts = [1usize, 2, 4];

    // Small burst: capacity large enough that queue never fills
    let mut group = c.benchmark_group("throughput_small_burst");
    for &n_threads in &thread_counts {
        let per_producer = items / n_threads;
        group.bench_function(format!("mpsc_queue2_{n_threads}p"), |b| {
            let queue = Arc::new(MpscQueue2::<u64>::new(LARGE_CAP));
            b.iter(|| run_workload(&queue, n_threads, per_producer));
        });
        group.bench_function(format!("crossbeam_array_queue_{n_threads}p"), |b| {
            let queue = Arc::new(ArrayQueue::new(LARGE_CAP));
            b.iter(|| run_workload_crossbeam(&queue, n_threads, per_producer));
        });
    }
    group.finish();

    // Large burst: capacity small enough that producers overwhelm the queue
    let mut group = c.benchmark_group("throughput_large_burst");
    for &n_threads in &thread_counts {
        let per_producer = items / n_threads;
        group.bench_function(format!("mpsc_queue2_{n_threads}p"), |b| {
            let queue = Arc::new(MpscQueue2::<u64>::new(SMALL_CAP));
            b.iter(|| run_workload(&queue, n_threads, per_producer));
        });
        group.bench_function(format!("crossbeam_array_queue_{n_threads}p"), |b| {
            let queue = Arc::new(ArrayQueue::new(SMALL_CAP));
            b.iter(|| run_workload_crossbeam(&queue, n_threads, per_producer));
        });
    }
    group.finish();
}

fn bench_push_latency(c: &mut Criterion) {
    let thread_counts = [1usize, 2, 4, 8];

    let mut group = c.benchmark_group("push_latency_busy");

    for &n_threads in &thread_counts {
        group.bench_function(format!("mpsc_queue2_{n_threads}p"), |b| {
            let queue = Arc::new(MpscQueue2::<u64>::new(LARGE_CAP));
            b.iter(|| {
                let queue = Arc::clone(&queue);
                let start = Arc::new(Barrier::new(n_threads));
                let mut handles = Vec::new();
                for _ in 0..n_threads {
                    let queue = Arc::clone(&queue);
                    let start = Arc::clone(&start);
                    handles.push(thread::spawn(move || {
                        start.wait();
                        queue.push(black_box(42));
                    }));
                }
                for handle in handles {
                    handle.join().unwrap();
                }
                // Drain them
                loop {
                    if queue.pop().is_none() {
                        break;
                    }
                }
            });
        });

        group.bench_function(format!("crossbeam_array_queue_{n_threads}p"), |b| {
            let queue = Arc::new(ArrayQueue::<u64>::new(LARGE_CAP));
            b.iter(|| {
                let queue = Arc::clone(&queue);
                let start = Arc::new(Barrier::new(n_threads));
                let mut handles = Vec::new();
                for _ in 0..n_threads {
                    let queue = Arc::clone(&queue);
                    let start = Arc::clone(&start);
                    handles.push(thread::spawn(move || {
                        start.wait();
                        let _ = queue.push(black_box(42));
                    }));
                }
                for handle in handles {
                    handle.join().unwrap();
                }
                loop {
                    if queue.pop().is_none() {
                        break;
                    }
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_throughput, bench_push_latency);
criterion_main!(benches);
