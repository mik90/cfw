use std::sync::{Arc, Barrier};
use std::thread;

use base::mpsc_queue::{ExperimentalMpscQueue, MpscQueue};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use crossbeam_queue::ArrayQueue;

const CAP: usize = 1_048_576;

// -- Workload runner -- //

fn run_workers<Q: Send + Sync + 'static>(
    queue: &Arc<Q>,
    n_producers: usize,
    per_producer: usize,
    push: fn(&Q, u64),
    pop: fn(&Q) -> Option<u64>,
) {
    let total = n_producers * per_producer;
    let start = Arc::new(Barrier::new(n_producers + 1));
    let done = Arc::new(Barrier::new(n_producers + 1));

    let mut handles = Vec::new();
    for tid in 0..n_producers {
        let q = Arc::clone(queue);
        let s = Arc::clone(&start);
        let d = Arc::clone(&done);
        handles.push(thread::spawn(move || {
            s.wait();
            for i in 0..per_producer {
                push(&q, (tid as u64) * (per_producer as u64) + i as u64);
            }
            d.wait();
        }));
    }

    start.wait();
    let mut count = 0;
    while count < total {
        if pop(queue).is_some() {
            count += 1;
            black_box(count);
        }
    }
    done.wait();
    for h in handles {
        h.join().unwrap();
    }
}

// -- Benchmarks -- //

fn bench_throughput(c: &mut Criterion) {
    let items = 1_000_000;
    let thread_counts = [1usize, 2, 4];

    let mut group = c.benchmark_group("throughput");
    for &n_threads in &thread_counts {
        let per_prod = items / n_threads;

        group.bench_function(format!("mpsc_crossbeam_{n_threads}p"), |b| {
            let queue = Arc::new(MpscQueue::<u64>::new(CAP));
            b.iter(|| run_workers(&queue, n_threads, per_prod, |q, v| { q.push(v); }, |q| q.pop()));
        });

        group.bench_function(format!("mpsc_experimental_{n_threads}p"), |b| {
            let queue = Arc::new(ExperimentalMpscQueue::<u64>::new(CAP));
            b.iter(|| run_workers(&queue, n_threads, per_prod, |q, v| { q.push(v); }, |q| q.pop()));
        });

        group.bench_function(format!("crossbeam_arrayqueue_{n_threads}p"), |b| {
            let queue = Arc::new(ArrayQueue::<u64>::new(CAP));
            b.iter(|| {
                run_workers(
                    &queue,
                    n_threads,
                    per_prod,
                    |q, v| {
                        while q.push(v).is_err() {
                            std::hint::spin_loop();
                        }
                    },
                    |q| q.pop(),
                )
            });
        });
    }
    group.finish();
}

fn bench_push_latency(c: &mut Criterion) {
    let thread_counts = [1usize, 2, 4, 8];
    let mut group = c.benchmark_group("push_latency");

    for &n_threads in &thread_counts {
        group.bench_function(format!("mpsc_crossbeam_{n_threads}p"), |b| {
            let q = Arc::new(MpscQueue::<u64>::new(CAP));
            b.iter(|| {
                let q = Arc::clone(&q);
                let start = Arc::new(Barrier::new(n_threads));
                let mut hs = Vec::new();
                for _ in 0..n_threads {
                    let q = Arc::clone(&q);
                    let s = Arc::clone(&start);
                    hs.push(thread::spawn(move || {
                        s.wait();
                        q.push(black_box(42));
                    }));
                }
                for h in hs {
                    h.join().unwrap();
                }
                q.clear();
            });
        });

        group.bench_function(format!("mpsc_experimental_{n_threads}p"), |b| {
            let q = Arc::new(ExperimentalMpscQueue::<u64>::new(CAP));
            b.iter(|| {
                let q = Arc::clone(&q);
                let start = Arc::new(Barrier::new(n_threads));
                let mut hs = Vec::new();
                for _ in 0..n_threads {
                    let q = Arc::clone(&q);
                    let s = Arc::clone(&start);
                    hs.push(thread::spawn(move || {
                        s.wait();
                        q.push(black_box(42));
                    }));
                }
                for h in hs {
                    h.join().unwrap();
                }
                q.clear();
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_throughput, bench_push_latency);
criterion_main!(benches);
