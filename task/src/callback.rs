use crate::time::FrameworkTime;
use crate::{Context, LoanError, Publisher, Subscriber};

/// Callback body and IO lifecycle, shared by live and deterministic executors.
/// Executors invoke `execute_callback` or `execute_callback_batch` rather than
/// calling `run` directly.
pub trait Callback: Send {
    // TODO(port-execution-logging): Restore execution-log hooks and replay metadata
    // for inputs, committed outputs, and events within the split callback lifecycle.
    #[cfg(feature = "iceoryx2")]
    fn take_iox2_events(&mut self) -> Vec<crate::iox2::Iox2EventRegistration> {
        Vec::new()
    }
    fn set_waker(&mut self, _wake: crate::wake::WakeHandle) {}
    fn has_pending_inputs(&self) -> bool {
        false
    }
    fn visit_channel_names(&self, _visit: &mut dyn FnMut(&str)) {}
    fn update_inputs(&mut self) {}
    /// Preflight readiness may include incoming queues. It must not consume them.
    fn required_inputs_available(&self) -> bool {
        self.required_inputs_ready()
    }
    /// Readiness of the prepared read buffers, excluding later publications.
    fn required_inputs_ready(&self) -> bool {
        true
    }
    fn run(&mut self, context: &Context) -> Result<(), LoanError>;
    fn finish_inputs(&mut self) {}
    fn flush_outputs(&mut self, _timestamp: FrameworkTime) {}
    fn discard_outputs(&mut self) {}
}

impl<F> Callback for F
where
    F: FnMut(FrameworkTime) -> Result<(), LoanError> + Send,
{
    fn run(&mut self, context: &Context) -> Result<(), LoanError> {
        self(context.now())
    }
}

/// Update inputs, check required inputs, execute, and publish successful outputs.
/// Returns false when required inputs are missing. Error/panic paths discard
/// pending outputs. Publications from earlier callbacks remain committed.
pub fn execute_callback(callback: &mut dyn Callback, context: &Context) -> Result<bool, LoanError> {
    let Some(prepared) = prepare_callback(callback) else {
        return Ok(false);
    };
    prepared.run(context)?.commit();
    Ok(true)
}

struct Invocation<'a> {
    callback: &'a mut dyn Callback,
    ran: bool,
}
impl Drop for Invocation<'_> {
    fn drop(&mut self) {
        self.callback.discard_outputs();
        if self.ran {
            self.callback.finish_inputs();
        }
    }
}

/// Exclusive, prepared input snapshot. Dropping it cancels pending outputs.
#[must_use]
struct PreparedCallback<'a>(Invocation<'a>);

/// Completed body with unpublished outputs. Drop cancels; commit publishes at
/// the invocation timestamp. The callback cannot be reused while this exists.
#[must_use]
struct CompletedCallback<'a> {
    invocation: Invocation<'a>,
    timestamp: FrameworkTime,
}

fn prepare_callback(callback: &mut dyn Callback) -> Option<PreparedCallback<'_>> {
    let invocation = Invocation {
        callback,
        ran: false,
    };
    invocation.callback.update_inputs();
    invocation
        .callback
        .required_inputs_ready()
        .then_some(PreparedCallback(invocation))
}
impl<'a> PreparedCallback<'a> {
    fn run(mut self, context: &Context) -> Result<CompletedCallback<'a>, LoanError> {
        self.0.ran = true;
        self.0.callback.run(context)?;
        Ok(CompletedCallback {
            invocation: self.0,
            timestamp: context.now(),
        })
    }
}
impl CompletedCallback<'_> {
    fn commit(self) {
        self.invocation.callback.flush_outputs(self.timestamp);
    }
}

#[derive(Debug)]
pub enum BatchFailure {
    Callback(LoanError),
    Panic,
}
#[derive(Debug)]
struct BatchError {
    position: usize,
    failure: BatchFailure,
}

#[derive(Debug)]
pub enum BatchExecutionError<E> {
    NoWorkers,
    Callback { index: usize, failure: BatchFailure },
    BeforeCommit(E),
}

/// Prepare every input snapshot, run ready callbacks with bounded parallelism,
/// then commit in the supplied order. Returns the caller-supplied IDs of callbacks
/// that ran and the value produced by `before_commit`.
///
/// `before_commit` runs after all bodies finish and before any output is published,
/// allowing an executor to validate timing for the entire batch. A body failure
/// or validation error cancels all unpublished outputs after workers join. User
/// state and consumed inputs are not rolled back. Preparation/validation/commit
/// panics propagate with the same cleanup; a commit panic cannot retract an
/// already-published prefix. Phase handles remain internal to this utility.
pub fn execute_callback_batch<'a, 'storage: 'a, R, E>(
    callbacks: impl IntoIterator<Item = (usize, &'a mut (dyn Callback + 'storage))>,
    context: &Context,
    workers: usize,
    before_commit: impl FnOnce(&[usize]) -> Result<R, E>,
) -> Result<(Vec<usize>, R), BatchExecutionError<E>> {
    if workers == 0 {
        return Err(BatchExecutionError::NoWorkers);
    }
    let mut executed = Vec::new();
    let mut prepared = Vec::new();
    for (index, callback) in callbacks {
        if let Some(invocation) = prepare_callback(callback) {
            executed.push(index);
            prepared.push(invocation);
        }
    }
    let completed = run_prepared_batch(prepared, context, workers).map_err(|error| {
        BatchExecutionError::Callback {
            index: executed[error.position],
            failure: error.failure,
        }
    })?;
    let result = before_commit(&executed).map_err(BatchExecutionError::BeforeCommit)?;
    for invocation in completed {
        invocation.commit();
    }
    Ok((executed, result))
}

/// Execute prepared callbacks with bounded parallelism, retaining input order.
/// No outputs are committed here. On failure, all successful results are dropped
/// and cancelled after workers join. User state and consumed inputs are not rolled back.
fn run_prepared_batch<'a>(
    prepared: Vec<PreparedCallback<'a>>,
    context: &Context,
    workers: usize,
) -> Result<Vec<CompletedCallback<'a>>, BatchError> {
    if prepared.is_empty() {
        return Ok(Vec::new());
    }
    let count = workers.min(prepared.len());
    let mut chunks: Vec<Vec<_>> = (0..count).map(|_| Vec::new()).collect();
    for (position, job) in prepared.into_iter().enumerate() {
        chunks[position % count].push((position, job));
    }
    let run_chunk = |jobs: Vec<(usize, PreparedCallback<'a>)>| {
        jobs.into_iter()
            .map(|(position, job)| {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run(context)));
                (
                    position,
                    match result {
                        Ok(result) => result.map_err(BatchFailure::Callback),
                        Err(_) => Err(BatchFailure::Panic),
                    },
                )
            })
            .collect::<Vec<_>>()
    };
    let mut results = if count == 1 {
        run_chunk(chunks.pop().unwrap())
    } else {
        std::thread::scope(|scope| {
            let handles: Vec<_> = chunks
                .into_iter()
                .map(|jobs| scope.spawn(move || run_chunk(jobs)))
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| {
                    handle
                        .join()
                        .expect("batch worker failed outside callback invocation")
                })
                .collect::<Vec<_>>()
        })
    };
    results.sort_by_key(|(position, _)| *position);
    results
        .into_iter()
        .map(|(position, result)| result.map_err(|failure| BatchError { position, failure }))
        .collect()
}

pub trait InputPorts {
    fn update(&self);
    fn finish(&self);
    fn set_waker(&mut self, wake: crate::wake::WakeHandle);
    fn has_pending(&self) -> bool;
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str));
}
pub trait OutputPorts {
    fn flush(&mut self, timestamp: FrameworkTime);
    fn discard(&mut self);
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str));
}
impl<T> InputPorts for Subscriber<'_, T> {
    fn finish(&self) {
        self.finish_iteration();
    }
    fn set_waker(&mut self, wake: crate::wake::WakeHandle) {
        Subscriber::set_waker(self, wake);
    }
    fn has_pending(&self) -> bool {
        self.requests_execution()
    }
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        visit(self.channel_name());
    }
    fn update(&self) {
        Subscriber::update(self);
    }
}
impl<T> OutputPorts for Publisher<'_, T> {
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        visit(self.channel_name());
    }
    fn flush(&mut self, timestamp: FrameworkTime) {
        Publisher::flush(self, timestamp);
    }
    fn discard(&mut self) {
        self.discard_pending();
    }
}
impl InputPorts for () {
    fn finish(&self) {}
    fn set_waker(&mut self, _wake: crate::wake::WakeHandle) {}
    fn has_pending(&self) -> bool {
        false
    }
    fn visit_channel_names(&self, _visit: &mut dyn FnMut(&str)) {}
    fn update(&self) {}
}
impl OutputPorts for () {
    fn visit_channel_names(&self, _visit: &mut dyn FnMut(&str)) {}
    fn flush(&mut self, _timestamp: FrameworkTime) {}
    fn discard(&mut self) {}
}
impl<A: InputPorts, B: InputPorts> InputPorts for (A, B) {
    fn finish(&self) {
        self.0.finish();
        self.1.finish();
    }
    fn set_waker(&mut self, wake: crate::wake::WakeHandle) {
        self.0.set_waker(wake.clone());
        self.1.set_waker(wake);
    }
    fn has_pending(&self) -> bool {
        self.0.has_pending() || self.1.has_pending()
    }
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        self.0.visit_channel_names(visit);
        self.1.visit_channel_names(visit);
    }
    fn update(&self) {
        self.0.update();
        self.1.update();
    }
}
impl<A: OutputPorts, B: OutputPorts> OutputPorts for (A, B) {
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        self.0.visit_channel_names(visit);
        self.1.visit_channel_names(visit);
    }
    fn flush(&mut self, timestamp: FrameworkTime) {
        self.0.flush(timestamp);
        self.1.flush(timestamp);
    }
    fn discard(&mut self) {
        self.0.discard();
        self.1.discard();
    }
}

/// Hand-written callback with typed IO bundles. The body decides how to handle
/// empty inputs; macro-generated required inputs additionally gate execution.
pub struct CallbackNode<I, O, F> {
    inputs: I,
    outputs: O,
    body: F,
}

impl<I, O, F> CallbackNode<I, O, F> {
    pub fn new(inputs: I, outputs: O, body: F) -> Self {
        Self {
            inputs,
            outputs,
            body,
        }
    }
}

impl<I, O, F> Callback for CallbackNode<I, O, F>
where
    I: InputPorts + Send,
    O: OutputPorts + Send,
    F: FnMut(&I, &mut O, &Context) -> Result<(), LoanError> + Send,
{
    fn finish_inputs(&mut self) {
        self.inputs.finish();
    }
    fn set_waker(&mut self, wake: crate::wake::WakeHandle) {
        self.inputs.set_waker(wake);
    }
    fn has_pending_inputs(&self) -> bool {
        self.inputs.has_pending()
    }
    fn visit_channel_names(&self, visit: &mut dyn FnMut(&str)) {
        self.inputs.visit_channel_names(visit);
        self.outputs.visit_channel_names(visit);
    }
    fn update_inputs(&mut self) {
        self.inputs.update();
    }
    fn run(&mut self, context: &Context) -> Result<(), LoanError> {
        (self.body)(&self.inputs, &mut self.outputs, context)
    }
    fn flush_outputs(&mut self, timestamp: FrameworkTime) {
        self.outputs.flush(timestamp);
    }
    fn discard_outputs(&mut self) {
        self.outputs.discard();
    }
}
