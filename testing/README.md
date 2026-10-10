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

let setup = builder.allocate();
let mut executor = setup.build();

input.send(21);
executor.step();
output.messages(&mut executor, |_, message| assert_eq!(message.message, 42));
```