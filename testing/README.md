# Callback unit testing

`UnitTestExecutor` runs a borrowed callback graph using deterministic simulation.
Declare fixture endpoints alongside task endpoints before allocating graph storage,
then pass their typed bindings to `UnitTestExecutorBuilder`.

```rust
use task::{ChannelPlan, GraphPlan, GraphBuilder, RequiredInput, Publisher, LoanError};
use task_macros::task_callback;
use testing::UnitTestExecutorBuilder;

struct Double;
#[task_callback]
impl Double {
    fn run(&self, input: RequiredInput<u64>, output: &mut Publisher<u64>) -> Result<(), LoanError> {
        output.publish(*input * 2)
    }
}

let mut input = ChannelPlan::new("input");
let mut output = ChannelPlan::new("output");
let declaration = Double::declare(&mut input, &mut output).unwrap();
let send_key = input.publisher(1);
let capture_key = output.subscriber(2);
let storage = GraphPlan::new((input, output)).allocate().unwrap();
let input = storage.channels().0.build();
let output = storage.channels().1.build();
let mut graph = GraphBuilder::with_storage(&storage);
graph.add_callback("double", || Ok(Double.bind(declaration, &input, &output)?));

let builder = UnitTestExecutorBuilder::new(graph.build().unwrap());
let mut sender = builder.add_test_publisher(input.take_publisher(&send_key).unwrap());
let mut capture = builder.add_test_subscriber(output.take_subscriber(&capture_key).unwrap());
let mut executor = builder.build();
sender.send(21);
assert_eq!(executor.step().executed, [0]);
drop(executor);
let retained = capture.take_messages();
drop((sender, capture));
assert_eq!(retained[0].message, 42);
```

- `with_config` accepts `UnitTestExecutorConfig` (the simulation configuration),
  including start time, virtual pools and real worker count. Callback schedules
  are set on `GraphBuilder`.
- `step` returns before/after time, executed callback indices and idle status.
  `try_step` reports failures and poisons the session after an error.
- Native fixture sends use the current simulated time. Captures drain in queue
  order; `try_messages` and `try_take_messages` report cumulative overflow counts.
  Retaining messages consumes arena capacity; reserve extra retained capacity on
  the channel plan when needed. `try_send` reports arena exhaustion.
- Fixture ports are usable once bound, including before executor construction.
  Native handles and retained messages borrow storage, not the executor. After
  execution ends, publishers retain the last simulated timestamp.
- With `iceoryx2`, bind IPC publisher/subscriber fixtures from an `Iox2ChannelPlan`.
  Data sends are silent; `add_iox2_test_notifier(channel)` stages counted events
  for the next step. Unknown event channels are reported by `try_step`. Notifier
  handles close when the builder/executor is dropped. IPC capture drains into
  owned messages and its bound middleware port can outlive execution.
