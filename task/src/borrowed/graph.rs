use std::collections::HashSet;

use super::LoanError;
use crate::time::FrameworkTime;

pub type FactoryError = Box<dyn std::error::Error + Send + Sync>;
type Callback<'storage> = Box<dyn FnMut(FrameworkTime) -> Result<(), LoanError> + Send + 'storage>;
type Factory<'build, 'storage> =
    Box<dyn FnOnce() -> Result<Callback<'storage>, FactoryError> + 'build>;

#[derive(Debug)]
pub enum GraphBuildError {
    DuplicateCallback(String),
    Factory {
        callback: String,
        source: FactoryError,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct GraphStepError {
    pub callback: String,
    pub source: LoanError,
}

/// Deferred callback construction after storage and typed bindings exist.
/// Factories can borrow temporary bindings for `'build`; returned callbacks only
/// retain their storage borrows for `'storage` and can move to scoped workers.
pub struct GraphBuilder<'build, 'storage> {
    factories: Vec<(String, Factory<'build, 'storage>)>,
}

impl<'build, 'storage> Default for GraphBuilder<'build, 'storage> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'build, 'storage> GraphBuilder<'build, 'storage> {
    pub fn new() -> Self {
        Self {
            factories: Vec::new(),
        }
    }

    pub fn add_callback<F, C>(&mut self, name: impl Into<String>, factory: F)
    where
        F: FnOnce() -> Result<C, FactoryError> + 'build,
        C: FnMut(FrameworkTime) -> Result<(), LoanError> + Send + 'storage,
    {
        self.factories.push((
            name.into(),
            Box::new(move || factory().map(|callback| Box::new(callback) as Callback<'storage>)),
        ));
    }

    /// Validate names before running factories. Failure drops all successfully
    /// constructed callbacks and the unexecuted factories by normal destruction.
    pub fn build(self) -> Result<BuiltGraph<'storage>, GraphBuildError> {
        let mut names = HashSet::new();
        for (name, _) in &self.factories {
            if !names.insert(name) {
                return Err(GraphBuildError::DuplicateCallback(name.clone()));
            }
        }
        let mut callbacks = Vec::with_capacity(self.factories.len());
        for (name, factory) in self.factories {
            let callback = factory().map_err(|source| GraphBuildError::Factory {
                callback: name.clone(),
                source,
            })?;
            callbacks.push((name, callback));
        }
        Ok(BuiltGraph { callbacks })
    }
}

/// Borrowed callback graph with an explicit insertion-order stepping operation.
/// This is a construction/lifecycle harness, not the readiness or timing scheduler.
/// Callback closures own their typed endpoints and perform update/flush operations.
pub struct BuiltGraph<'storage> {
    callbacks: Vec<(String, Callback<'storage>)>,
}

impl BuiltGraph<'_> {
    /// Stop at the first error. Earlier callbacks may already have published;
    /// the step is not transactional and later callbacks are not executed.
    pub fn step(&mut self, timestamp: FrameworkTime) -> Result<(), GraphStepError> {
        for (name, callback) in &mut self.callbacks {
            callback(timestamp).map_err(|source| GraphStepError {
                callback: name.clone(),
                source,
            })?;
        }
        Ok(())
    }
}
