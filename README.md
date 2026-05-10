# cfw 

**C**allback **f**rame**w**ork

A testbed for how _I_ would like a robotics task framework to work.

This is very much a work in progress, it'll be barely tested, undocumented, and, sloppy for a while (forever?).

The goal is to make an integration-agnostic framework that can plug into a bunch of different ecosystems while providing
- a featureful user-facing API for pub/sub that allows the framework to manage when a callback should run
- deterministic multi-threaded simulation (same inputs == same outputs)
- exact replay: given a commit that produced a log, you can reproduce the exact messages that were seen in the log (but with more debugging info)
- convenient unit-testing API for tasks

If you want a Rust task framework with actual integrations and meaningful support, you should use https://github.com/copper-project/copper-rs.

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
- [ ] forwarded messages, publish a message alongside a handle to a message it was produced with
  - [x] start on this`
    - so, given a message B that was produced using contents of A, allow for message B to link to message A
    - this should allow us to defer logging of messages to a normal callback, we'd publish a message that has some handles and the timestamp in which they were seenA
    - [x] handle message headers, dont need to embed this in ArenaPtr, we can just have ArenaPtr take in a message + header
    - [ ] integrate with Input/Output types
- [ ] exact replay executor
  - prob should mimic thread pool constraints but we should be able to execute stuff in any order as long as callbacks aren't stateful
  - I'll need to consider logging executions and message queues for this
  - Should allow for reproduction of a given message
  - populate all intermediate non-logged messages
  - [ ] optionally log executions via publisher in other (threadpool / sim executor)
  - [ ] see if we can avoid more allocations with Vecs in logging
  - [ ] create workflow to log data
  - [ ] add code to parse logged data and replay specific executions
- [ ] provide a stop message that the executor can subscribe to
- [ ] allow for foreign subscribers/publishers such as iceoryx2
  - we should be able to swap out foreign/native impls per task at build/configuration time
  - arena configuration may be different per backing pub/sub system
  - the reason we'd do this instead of introducing another callback that publishes on a given channel is that another callback means we have another queue whose capacity we have to manage
  - blocking pub/sub is fine here since people can opt into it, if they want async pub/sub they can have a separate callback to do the work
- [ ] maybe a manual input pull and fallable publish? allows for global defaults and configurable 
  - storing the typed publisher or subscriber might work here
  - the proc macro setup isn't required, so this may be a non-proc macro way to implement things
- [ ] allow for run_generic on Fn and FnMut
- [ ] provide task storage abstraction
  - cleanup subscriber buffers before publishers
  - allow for indexing with some strong types
  - but will this be portable across all use-cases?
- [ ] unit test executor
  - allows for testing whole tasks in unit test, based on sim executor
- [ ] dump connections in graphviz or some other diagram tool
- [ ] live replay executor
  - similar to live executor, except it works by publishing messages from a log at some given speed multiplier
  - useful for development of viz tools
- [ ] flesh out callback construction and how we want to handle configuration
- [ ] do some better testing to ensure that users can't hold onto reference of messages in the pub/sub system
- [ ] see if I should use pins in the arenas to avoid moving MaybeUninit, the current setup may suffice
- [ ] add tests to make sure we aren't dynamically allocating after everything is connected
- [ ] more consistent handling of MaybeUninit in subscriber, maybe specialize on it in read buffer?
- [ ] better stress testing for multi-threaded determinism in sim 

## Debugging tips

To view proc macro output of the unit test in test.rs

```bash
cargo expand --test test > ~/Downloads/temp.rs
code ~/Downloads/temp.rs
```

To view live output of test binary

```bash
cargo t test_thread_pool_exec -- --nocapture
```

Run tests in miri (these should pass)

```bash
cargo +nightly miri test
```
