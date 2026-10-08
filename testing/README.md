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
    fn run(&self, input: RequiredInput<u64>, output: &mut Publisher<u64>) -> Result<(), LoanError> {
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

For ordinary control flow outside a closure, keep the allocated setup in scope:

```text
let setup = builder.allocate();
let mut executor = setup.build();

input.send(21);
executor.step();
output.messages(&mut executor, |_, message| assert_eq!(message.message, 42));
```

- Channels default to callback argument names; `#[channel("name")]` overrides
  them. Matching task ports and fixtures are connected automatically. Named
  construction supports owned (`'static`) payload types without requiring Clone.
- Input handles can be captured individually or in collections. Sends queue owned
  values at the current simulated timestamp. Each step drains fixtures in
  registration order, FIFO within each fixture, before running callbacks.
- Output inspection is synchronous and borrowed. Each call drains a batch and
  restarts its index at zero. Native captures default to capacity 10; use
  `add_test_subscriber_with_capacity` to change it and `try_messages` to inspect
  cumulative overflow counts instead of asserting that no messages were dropped.
- `add_scheduled_task(name, task, duration, schedule)` accepts a `CallbackSchedule`
  for periods, virtual pools, startup and custom next deadlines. The required
  duration may be a `Duration` or `ExecutionDuration::dynamic(...)`. Explicit zero
  is allowed. Publications use invocation time; modeled durations occupy pools
  and affect simulated time advancement. `with_config` configures start time,
  virtual pool sizes and real worker count.
- `allocate`, `build`, `run`, and `step` panic on failure; their `try_` variants
  return errors. `run` returns the closure's result and propagates assertion
  panics. A setup builds one executor. Input handles close on executor destruction
  or failed execution; output inspection remains available after a failed step.
- IPC equivalents are `add_iox2_test_publisher`, `add_iox2_test_subscriber`, and
  `add_iox2_test_notifier`. IPC data sends are silent; counted notifications are
  injected separately. IPC output inspection also supports non-Clone payloads;
  middleware overflow counts are not exposed by this capture API.
- `BoundUnitTestExecutorBuilder` supports explicitly bound endpoint fixtures for
  low-level storage/lifetime tests and payload types containing storage borrows.
