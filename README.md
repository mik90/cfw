# cfw 

**C**allback **f**rame**w**ork

A testbed for how _I_ would like a robotics task framework to work.

This is very much a work in progress, and I'll be changing things frequently since it's more of a test bed although things should still generally work.

The goal is to make an integration-agnostic framework that can plug into a bunch of different ecosystems while providing
- a featureful user-facing API for pub/sub that allows the framework to manage when a callback should run
- deterministic multi-threaded simulation (same inputs == same outputs)
- exact replay: given a commit that produced a log, you can reproduce the exact messages that were seen in the log (but with more debugging info)
- convenient unit-testing API for tasks

If you want a Rust task framework with actual integrations and meaningful support, you should use https://github.com/copper-project/copper-rs.

This is a mix of human code and LLM-generated code depending on how much I wanted to build something out myself.
However, this README is still human-maintained (although bots are able to check off TODO items).

## Overview

cfw provides a pub/sub messaging framework that is combined with callback execution. This allows user code to leverage the framework for structured concurrency.
It's useful to delegate concurrency to a framework in some domains (robotics, automotive, etc) so that it can be modelled consistently between onboard execution and simulation. 
There are other benefits to structured concurrency of course, (less likely to hit data races) but there's a difference between avoiding a race at runtime and trying to reproduce exactly what happened in concurrent execution. The former we consider a feature we get for free, but the latter encompasses more and should be the main goal of a concurrent robotics framework.

I use "task" and "callback" interchangeably and don't feel like consolidating the two yet. I suppose that a "task" can be a "callback" based on how the code has played out. But we could consider a task as a group of callbacks. A task is definitely not a process though, as many task frameworks tend to define it.

Related to that, users are meant to keep callbacks in a single process as much as is reasonable for their use-case.
Processes tend to have some level of base cost when it comes to either OS scheduling or whatever accoutrements are added to the infrastructure. 
A heavily multi-process system's benefit is mainly in the realm of
- minimizing impact when a process crashes
- configuring OS permissions 

For the former, I'm hoping that Rust's memory safety can minimize the impact of crashing. It is up to users to avoid panics, though.
For the latter, I can't really do anything about this.

### Task API

The task API is best supported (for better or worse) through a proc macro in [task_macros](./task_macros). This allows for the most concise declaration at the expense of proc macros being hard to reason about.

Being a WIP, this is certainly liable to change at any point, and I'm omitting some annoying details but this is the core API:

```rust
pub struct FizzBuzzCalculator {}

#[task_callback] // Marked on impl block, as the macro inserts some `fn`s
impl FizzBuzzCalculator {
    // User declares a `run`, and the inputs/outputs are described by the function signature
    fn run(
        &mut self, // Can take mut self, although shouldn't be strictly necessary
        // For each input/output, users declare a type from input.rs or output.rs
        // Default channels can be set, and the input/output behavior is declared through the type
        
        // This integer input is required for running, and defaults to the INTEGER_CHANNEL
        #[channel(FizzBuzzTaskInfo::INTEGER_CHANNEL)] integer: RequiredInput<u64>,
        // We have a single output channel (defaulted to fizz buzz)
        #[channel(FizzBuzzTaskInfo::FIZZ_BUZZ_STRING_CHANNEL)] mut fizz_buzz_string: Output<String>,
    ) {
        // Input can be accessed through Deref, although both header/payload are available
        let is_fizz = (*integer).is_multiple_of(3);
        let is_buzz = (*integer).is_multiple_of(5);
        let is_fizz_buzz = is_fizz && is_buzz;

        if is_fizz_buzz {
            // Although we happen to use a dynamically allocated string here, all pub/sub types are allocated in arenas
            // so if a contiguous type is used, users can leverage zero-copy messaging without dynamic memory allocation.
            // Here, we could do that by using a fixed size string.
            *fizz_buzz_string = String::from("FizzBuzz");
        } else if is_fizz {
            *fizz_buzz_string = String::from("Fizz");
        } else if is_buzz {
            *fizz_buzz_string = String::from("Buzz");
        } else {
            *fizz_buzz_string = integer.to_string();
        }
        fizz_buzz_string.send();
    }

```

Underneath the macro API, there is a non-macro [Callback trait](task/src/callback.rs) that the macro implements.
This _can_ be implemented manually although it's tedious to do so. The lack of variadic generic support in Rust makes you need to lean on macros, so I don't really know a way around this.

The `Callback` trait exposes an interface for callbacks to
- iterate over all subscribers/publishers
- flush all subscribers into the more friendly Input types
- actually trigger the user-defined callback
- flush all publisher loans into the pub/sub system

### Task building API

This is the part I'm least happy with, and would like to change.

Currently, each task definition requires a `callback_builder()` function that allows the user to set the expected execution time or any time-based execution behavior (periodics).

On top of that, we have a `CallbackBuilder` type that allows for swapping out channel names and other frequently changed components of a callback.

On top of _that_, there is a `TaskGraphBuilder` that allows for composing pre-configured callbacks and `CallbackBuilder`s. 
The `TaskGraphBuilder` is more inline with what I want to support, it's just the intermediate layers between this top level and the lower levels that I'm less settled on..

The `TaskGraphBuilder` lets us compose many callbacks together alongside where they live in a variable count of thread pools.
Additionally, it's where we can layer on `TaskGraphBuildStep`s. This trait allows code to iterate over all existing callbacks (including channels) so that it can insert new callbacks. This allows us to inject logging or diagnostic handling callbacks that may subscribe to every channel in the graph, or channels with certain names/types, or something else entirely. 

The main difficulty with build steps is that, as we layer them, there may be a cyclic dependency. For example, the logging build step may have a diagnostic channel that should be handled. But the diagnostic build step itself has a channel that should be logged. We need to run both, but no order is correct since it's a cycle.
We can fix this by either
1. having build steps declare some static channels that are present regardless of ordering
2. running build steps multiple times, kind of like how compiler optimization passes can be run multiple times to piggyback off each other

Haven't figured out what I want yet, I'd lean towards 2 but then there's the question of "what if you have an infinite loop of channels being created"?

I think the best solution is to have a validation pass after the graph is built.

### Executors

Different use-cases call for different executors.
[ROS has a few types of executors](https://docs.ros.org/en/lyrical/ROS-Framework/client-libraries/About-Executors/About-Executors.html#types-of-executors) although the newer "Callback Group Events Executor" is the most similar to what cfw has modelled in each of its executors.

cfw has a single idea of an executor and then models it in vaguely the same way for each intended use-case

#### [live_executor](./live_executor)

This executor runs off the wall-clock and is what you'd run onboard a robot.
It schedules callbacks greedily and the execution order is non-deterministic.
Running this in tests is going to be flaky behavior-wise, although it's entirely possible to build tasks agnostic towards execution order and is generally useful to do so for the purpose of testing the framework.

#### [live_replay_executor](./live_replay_executor)

This executor is dependant on `live_executor` with a mild twist. It can run at a time-multiplier (generally you'd want it _slower_ than 1x) and is useful for local workflows where you want to either run a task slowly or play back logged data slowly and get a loose idea of how it works.

This is best for testing visualization tools or quick debugging, but not much else.

#### [exact_replay_executor](./exact_replay_executor)

This executor is meant for triaging/reproducing an issue seen on the robot.
Exact replay requires a log that contains execution logs which are produced by the frame during execution (surprisingly).
Generally we can assume this executor is replaying execution from the live_executor, but it can theoretically reproduce executions from any executor.

An executor can be configured to emit [execution logs](./task/src/execution_log.rs) which track
- the time the callback executed
- what inputs the callback executed with
- what outputs the callback emitted

Executions logs are meant to be shallow, so they only track the _headers_ of the messages that were used. It is up to a logger to actually log those messages and this replay executor performas a lookup of header->message in order to replay executions.

This saves a lot of logging bandwidth since a callback may expect some required input at startup and leave it in its input queue for the entire duration of execution.

#### [simulation_executor](./simulation_executor)

This executor is built to be deterministic across runs. So, given a set of inputs (synthetic or log based), it will result in the same exact execution order assuming that all the user-provided callbacks are deterministic.

The execution order may differ from any given execution from the live executor, this is meant to be a loose model that provides some insight into how code will behave in the live_executor but provides determinism so that simulation runs between commits can be robustly compared.

Execution occurs within a serialized step, where the executor basically calls step in a cycle and within a step, fans out execution across a fixed set of threads and then joins.
Originally I used Rayon with `par_iter` but I hit some Miri violation inside of Rayon during thread bringup so I ended up hand-rolling the worker threads to avoid it `¯\_(ツ)_/¯`.

Even though the execution threads are parallel, they can still model the thread-pooling of a live executor where certain callbacks may not be executed at the same time since they share a single threaded 'virtual' thread pool. So, if two callbacks `Foo` and `Bar` are on a single-threaded virtual pool, they should never be executed at the same simulation time. If they have two threads on that virtual pool, they are allowed to be executed at the same simulation time.

Splitting the notion of executor pools and virtual pools allows the executor to tune for CPU resources and wall-clock execution time while still modelling a given live executor setup.

As for inputs, the simulation executor can allow for some synthetic inputs which are generated within the graph or via an API to some external simulator program. An external program would need to be integrated via some callback that allows the external program to work within the bounds of cfw.

This may make more sense as I describe the log based simulation support. The simulation executor provides a [log simulation](simulation_executor/src/log_simulation.rs) utility where a callback reads in a log with some look-ahead, and requests execution times depending on the time of the logged messages. This lets the log callback drive hte simulation time forward since it'll keep requesting logging times in the 'future' as the log moves ahead.


#### [unit_test_executor](./testing/src/unit_test_executor.rs)

Unit testing at the task API is useful for bringing up a task and making sure it behaves as you'd expect it to behave. 
This executor is specific to unit tests and is built on the simulation executor since it allows for fixed execution order and avoids the native flakiness of the live executor.

Users are able to create test publishers to inject messages and test subscribers to listen to messages.
The workflow generally goes:
1. Create unit test executor builder
2. Add your callbacks under test to the executor
3. Create test publishers to inject messages
4. Create test subscribers to listen to messages
5. Build the executor
5. Inject some messages
6. Step the executor
7. Query the test subscribers and assert on the values
8. Repeat steps 5-7 until happy

### Logging

Honestly, the logging is bit of an afterthought. I've implemented a really naive approach based on `serde_json` which is super inefficient. The logging here is really just so I can iterate on the other components since logging is required to do exact replay.

I'd like this part to be modular so that users could implement their own serialization support and logging APIs that are more efficient.

There is a notion of "event logging" and "continuous logging". One big WIP item is supporting execution logs in event logging.
Contiuous logging assumes that all messages will be written to disk, so lookup of any header in an execution log should succeed.

Event logging assumes that only a subset of messages will be logged to disk whenever a user-determined "event" has occured. This is a  common workflow in various robotics/automotive/avionics/rail systems since these systems may be running for a very long time and there is only so much disk space/lifespan.
This makes shallow execution logging much harder, since the logger must have a notion of what messages callbacks are actually using so that it can log them. If a callback logs an execution that says "I ran with inputs A, B, and C", then the event logger must ensure that it has logged "A, B, and C" regardless of when those messages were seen in the system. So it'll likely need a lookup table of all execution logs to whether those headers were already logged.

This excution log lookup is something I haven't built yet in the logging task, but it's something I'd like to build.

In both "event logging" and "continuous logging", we also have to handle the case where messages are dropped. Arenas are fixed size at task graph build, and the logger can only run so frequently to consume messages. There is not infinite CPU, memory, or time so we have to either drop or block the entire graph until the CPU can catch up. Blocking the whole graph is not possible, as logging is treated like any other callback, so it is up to the user to tune the logging queue sizes accordingly to avoid drops and to handle drops as they find fit.

## TODO 

In order of priority

- [x] readiness check done on either publisher side (locking) or queued for running and checked then. It'd be best to avoid running when we know it won't be ready
      - on publisher side, it could query some atomic bitmask owned by the subscribing callback or some atomic value
- [x] allow for multiple thread pools in one process, could do multiple executors or somehow manage all this in one executor
  - one executor allows me to better utilize the periodic thread, so id rather have a mapping of callbacks to pools in one executor
- [x] provide current time to callbacks via context object
- [x] simulation executor
  - [x] Start w/ single thread, but consider multi-thread
  - [x] quick test for determinism
  - [x] honor thread pool constraints when it comes to whether a task is able to be executed in a group
  - [x] multi-thread execution, flush loans at end of step
- [x] create custom time type, SystemTime doesn't make sense (we'd want monotonic), Instant doesn't have enough flexibility (no default init, no max value), third party crates are probably not worth it
- [x] forwarded messages, publish a message alongside a handle to a message it was produced with
  - [x] start on this`
    - so, given a message B that was produced using contents of A, allow for message B to link to message A
    - this should allow us to defer logging of messages to a normal callback, we'd publish a message that has some handles and the timestamp in which they were seenA
    - [x] handle message headers, dont need to embed this in ArenaPtr, we can just have ArenaPtr take in a message + header
    - [x] integrate with Input/Output types
- [x] integrate `ArenaReaderPtr`  to reduce number of `unsafe` sites
  - still some `unsafe`s we could centralize better, but at least the subscriber doesn't have to do it now
- more advanced forwarding configurations
  - [x] forward message without extra data (so, as-is onto another channel)
    - maybe we can just use `()` as the T?
  - [ ] forwarded message that includes messages from multiple subscribers (with or without extra data)
    - [ ] probably need declarative macros and have something like `ForwardedMessageTuple${N}` for each arity
- [x] exact replay executor
  - prob should mimic thread pool constraints but we should be able to execute stuff in any order as long as callbacks aren't stateful
  - I'll need to consider logging executions and message queues for this
  - Should allow for reproduction of a given message
  - populate all intermediate non-logged messages
  - [x] optionally log executions via publisher in other (threadpool / sim executor)
  - [x] see if we can avoid more allocations with Vecs in logging
  - [x] create workflow to log data
  - [x] add code to parse logged data and replay specific executions
  - [x] configuration to log only execution duration (so, execution log enum for options)
- [x] live replay executor
  - similar to live executor, except it works by publishing messages from a log at some given speed multiplier
  - useful for development of viz tools
- [x] add log replay task to simulation executor to run over logged data
- [ ] allow for foreign subscribers/publishers such as iceoryx2
  - we should be able to swap out foreign/native impls per task at build/configuration time
  - arena configuration may be different per backing pub/sub system
  - the reason we'd do this instead of introducing another callback that publishes on a given channel is that another callback means we have another queue whose capacity we have to manage
  - blocking pub/sub is fine here since people can opt into it, if they want async pub/sub they can have a separate callback to do the work
- [x] provide task storage abstraction
  - cleanup subscriber buffers before publishers
  - allow for indexing with some strong types
  - shared-vec indexing over `Arc<RefCell<CallbackNode>>`: the storage is owned
    by one coordinating thread and hands worker threads their own clones
- [x] unit test executor (in `testing`)
  - allows for testing whole tasks in unit test, based on sim executor
- [x] dump connections in graphviz or some other diagram tool
  - just dumped via Display
- [x] flesh out callback construction and how we want to handle configuration
- [ ] do some better testing to ensure that users can't hold onto reference of messages in the pub/sub system
- [x] add tests to make sure we aren't dynamically allocating after everything is connected
  - live executor has some no_alloc tests
- [x] inputs should just store ref or optional ref of type instead of entire subscriber
- [x] readiness shouldn't be limited to 64 inputs, use an array or vec or something to contain many atomic bitsets
  - not ideal, but we only track readiness on required inputs, optional ones are not tracked
- [ ] log queue capacity shouldn't have a default value of 10, and we should have better configuration for per channel configuration
- [x] register loggable types even without macro usage (use register_channels)
- [ ] work in experimental mpsc queue
- [x] interned channel names and callback names to avoid allocations
    - use arena type, maybe leak at startup?
    - we could write mapping of intern ID to values to disk at build time
    - mainly relevant for publishing channel names at runtime

## Testing

Run tests including linting and miri

```bash
./run_test.sh
```

## Debugging tips

To view proc macro output of the unit test in test.rs

```bash
cargo expand --test test > ~/Downloads/temp.rs
code ~/Downloads/temp.rs
```
