## TODO 

In order of priority

- [x] readiness check done on either publisher side (locking) or queued for running and checked then. It'd be best to avoid running when we know it won't be ready
      - on publisher side, it could query some atomic bitmask owned by the subscribing callback or some atomic value
- [x] allow for multiple thread pools in one process, could do multiple executors or somehow manage all this in one executor
  - one executor allows me to better utilize the periodic thread, so id rather have a mapping of callbacks to pools in one executor
- [x] provide current time to callbacks via context object
- [ ] simulation executor
  - [x] Start w/ single thread, but consider multi-thread
  - [x] quick test for determinism
  - [x] honor thread pool constraints when it comes to whether a task is able to be executed in a group
  - [x] multi-thread execution, flush loans at end of step
- [ ] bitwise replay executor
  - Threading is kind of irrelevant here
  - I'll need to consider logging executions and message queues for this
  - Should allow for reproduction of a given message
  - populate all intermediate non-logged messages
  - [x] optionally log executions via publisher in other (threadpool / sim executor)
  - [ ] see if we can avoid more allocations with Vecs in logging
  - [ ] create workflow to log data
  - [ ] add code to parse logged data and replay specific executions
- [ ] normalize stop signal type between executors
- [ ] provide a stop message that the executor can subscribe to
- [ ] allow for foreign subscribers/publishers such as iceoryx2
  - we should be able to swap out foreign/native impls per task at build/configuration time
  - arena configuration may be different
- [ ] maybe a manual input pull and fallable publish? allows for global defaults and configurable 
  - storing the typed publisher or subscriber might work here
- [ ] forwarded messages, publish a message alongside a handle to a message it was produced with
      - so, given a message B that was produced using contents of A, allow for message B to link to message A
- [ ] allow for run_generic on Fn and FnMut
- [ ] provide task storage abstraction
  - cleanup subscriber buffers before publishers
  - allow for indexing with some strong types
  - but will this be portable across all use-cases?
- [ ] unit test executor
  - allows for testing whole tasks in unit test, based on sim executor
- [ ] create custom time type, SystemTime doesn't make sense (we'd want monotonic), Instant doesn't have enough flexibility (no default init, no max value), third party crates are probably not worth it
- [ ] dump connections in graphviz or some other diagram tool
- [ ] better stress testing for multi-threaded sim
- [ ] live replay executor
  - similar to live executor, except it works by slowing down replay speed

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
