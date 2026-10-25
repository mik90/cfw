# Callback unit testing

Register tasks and named fixtures, then run a deterministic simulation. Every task
requires an explicit execution duration. Fixtures and storage are cleaned up on
scope exit, including when an assertion panics.

```rust
use std::time::Duration;
use task::{RequiredInput, Publisher, LoanError};
use task_macros::task_callback;
use testing::UnitTestExecutorBuilder;

struct Double;
#[task_callback]
impl Double {
    fn run(&self, #[trigger(true)] input: RequiredInput<u64>, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(*input * 2)
    }
}

let mut builder = UnitTestExecutorBuilder::new();
builder.add_task("double", Double, Duration::from_micros(100));
let mut input = builder.add_test_publisher::<u64>("input");
let output = builder.add_test_subscriber::<u64>("output");

builder.run(|mut executor| {
    input.send(21);
    executor.step();
    assert_eq!(output.messages(&mut executor, |index, message| {
        assert_eq!(index, 0);
        assert_eq!(message.message, 42);
    }), 1);
});
```

For ordinary control flow outside a closure, keep the allocated `UnitTestStorage` in scope.
It owns channel storage and the single-use task/fixture construction state; its
executor borrows that storage:

```text
let storage = builder.allocate();
let mut executor = storage.build();

input.send_at(task::time::FrameworkTime::from_nanoseconds(1_000_000), 21);
executor.run_until_idle(16).unwrap();
output.messages(&mut executor, |_, message| {
    assert_eq!(message.message, 42);
    assert_eq!(message.header.published_at.to_nanoseconds(), 1_000_000);
});
```

## Scheduled input and execution

`send_at(time, value)` delays publication until simulated time reaches the requested
time. `send(value)` uses the current simulated time. Native and IPC named input
handles support both; named event handles provide `notify_at(time, event_id, count)`.
Invalid or past timestamps are rejected. Publications at equal times retain their
send order within each publisher; different fixture queues are registered in fixture
declaration order. Pending requests are incorporated before each executor step.

`step()` executes one scheduled batch, not the whole graph. `run_until_idle(max_steps)`
drives finite input sequences to completion. For periodic workloads, use
`run_until(time, max_steps)`: it executes work strictly before the boundary and leaves
work due exactly at that time for the next step. Both helpers retain the normal
fixture clock and failure handling.

Multiple ready callbacks start at the same timestamp when their virtual pools have
capacity. Configure `UnitTestExecutorConfig::virtual_pool_threads` to model that
capacity, and give each task its intended duration and pool. Input arrival does not
guarantee immediate execution when a pool is occupied. Downstream callbacks awakened
by a batch can run in another batch at the same timestamp if capacity remains.

Low-level tests can construct the same executor from an existing graph with
`UnitTestExecutor::new_with_graph(graph, config)`, or use `try_new_with_graph` for
fallible construction. Attach already-reserved native or IPC fixture endpoints
directly to the executor:

```text
let mut executor = UnitTestExecutor::new_with_graph(graph, config);
let mut input = executor.add_test_publisher(publisher);
let mut output = executor.add_test_subscriber(subscriber);
input.send_at(timestamp, value);
executor.run_until_idle(100).unwrap();
```

Fixture handles borrow channel storage rather than the executor. Start time and
virtual pools belong in `UnitTestExecutorConfig`; callback schedules belong in task
declarations or `BuiltGraph::configure_schedules`. Portless sources need startup or
timed schedules. All execution uses the simulation scheduler.

```rust
use std::time::Duration;
use task::{RequiredInput, Publisher, LoanError};
use task_macros::task_callback;
use testing::UnitTestExecutorBuilder;

struct Double;
#[task_callback]
impl Double {
    fn run(&self, #[trigger(true)] input: RequiredInput<u64>, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(*input * 2)
    }
}

let mut builder = UnitTestExecutorBuilder::new();
builder.add_task("double", Double, Duration::from_micros(100));
let mut input = builder.add_test_publisher::<u64>("input");
let output = builder.add_test_subscriber::<u64>("output");

let storage: testing::UnitTestStorage = builder.allocate();
let mut executor = storage.build();

input.send(21);
input.send_at(task::time::FrameworkTime::from_nanoseconds(1_000_000), 22);
executor.run_until_idle(16).unwrap();
assert_eq!(output.messages(&mut executor, |index, message| {
    assert_eq!(message.message, if index == 0 { 42 } else { 44 });
    assert_eq!(message.header.published_at.to_nanoseconds(), if index == 0 { 0 } else { 1_000_000 });
}), 2);
```
