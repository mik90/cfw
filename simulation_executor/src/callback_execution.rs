use task::callback::{CompletedCallback, PreparedCallback, prepare_callback};
use task::{Callback, Context, LoanError};

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
