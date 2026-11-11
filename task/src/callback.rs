use std::fmt::Debug;

use crate::execution_log_schema::{LoggedIox2Event, LoggedMessage};
use crate::time::FrameworkTime;
use crate::{Context, LoanError};
mod endpoints;
mod node;
pub use endpoints::*;
pub use node::CallbackNode;

/// Failure while resetting callback inputs or staging recorded events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackError {
    /// The callback cannot discard its buffered inputs.
    ResetUnsupported,
    /// No IPC event endpoint exists at the requested ordinal.
    UnknownEventEndpoint,
    /// The endpoint rejected the staged event.
    EventStaging(LoanError),
}
impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ResetUnsupported => f.write_str("callback cannot reset buffered inputs"),
            Self::UnknownEventEndpoint => f.write_str("unknown IPC event endpoint"),
            Self::EventStaging(error) => write!(f, "event staging failed: {error:?}"),
        }
    }
}
impl std::error::Error for CallbackError {}

/// Callback body and IO lifecycle, shared by live and deterministic executors.
/// Executors invoke `execute_callback` or `execute_callback_batch` rather than
/// calling `run` directly.
pub trait Callback: Send {
    /// Whether all outputs are known to avoid external transport delivery.
    fn outputs_are_local(&self) -> bool {
        false
    }
    /// Discard prepared and buffered inputs so an external snapshot can be injected.
    fn reset_inputs(&mut self) -> Result<(), CallbackError> {
        Err(CallbackError::ResetUnsupported)
    }
    /// Stage a counted IPC event on the endpoint at `ordinal`.
    fn stage_iox2_event(
        &mut self,
        _ordinal: usize,
        _event_id: usize,
        _count: u64,
    ) -> Result<(), CallbackError> {
        Err(CallbackError::UnknownEventEndpoint)
    }
    /// Whether this callback has endpoints on a non-native transport.
    fn has_iox2_endpoints(&self) -> bool {
        false
    }
    /// Whether any input can request execution without a timer.
    fn has_triggering_inputs(&self) -> bool {
        false
    }
    fn recording_endpoints(&self) -> Option<Vec<crate::execution_log_schema::EndpointDescriptor>> {
        None
    }
    fn visit_prepared_messages(
        &self,
        _visit: &mut dyn FnMut(crate::execution_log_schema::LoggedMessage),
    ) {
    }
    fn visit_pending_messages(
        &self,
        _visit: &mut dyn FnMut(crate::execution_log_schema::LoggedMessage),
    ) {
    }
    fn visit_prepared_iox2_events(
        &self,
        _visit: &mut dyn FnMut(crate::execution_log_schema::LoggedIox2Event),
    ) {
    }
    fn visit_pending_iox2_events(
        &self,
        _visit: &mut dyn FnMut(crate::execution_log_schema::LoggedIox2Event),
    ) {
    }
    #[cfg(feature = "iceoryx2")]
    fn take_iox2_events<'a>(
        &mut self,
        _visit: &mut dyn FnMut(crate::iox2::Iox2EventRegistration<'a>),
    ) where
        Self: 'a,
    {
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
    /// Buffered message count across required inputs, or `None` when the
    /// callback has no required inputs.
    fn required_input_count(&self) -> Option<usize> {
        None
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
    /// Whether all outputs are known to avoid external transport delivery.
    fn outputs_are_local(&self) -> bool {
        false
    }
    /// Discard prepared and buffered inputs so an external snapshot can be injected.
    fn reset_inputs(&mut self) -> Result<(), CallbackError> {
        Err(CallbackError::ResetUnsupported)
    }
    /// Stage a counted IPC event on the endpoint at `ordinal`.
    fn stage_iox2_event(
        &mut self,
        _ordinal: usize,
        _event_id: usize,
        _count: u64,
    ) -> Result<(), CallbackError> {
        Err(CallbackError::UnknownEventEndpoint)
    }
    /// Whether this callback has endpoints on a non-native transport.
    fn has_iox2_endpoints(&self) -> bool {
        false
    }
    fn recording_endpoints(&self) -> Option<Vec<crate::execution_log_schema::EndpointDescriptor>> {
        None
    }
    fn visit_prepared_messages(
        &self,
        _visit: &mut dyn FnMut(crate::execution_log_schema::LoggedMessage),
    ) {
    }
    fn visit_pending_messages(
        &self,
        _visit: &mut dyn FnMut(crate::execution_log_schema::LoggedMessage),
    ) {
    }
    fn visit_prepared_iox2_events(
        &self,
        _visit: &mut dyn FnMut(crate::execution_log_schema::LoggedIox2Event),
    ) {
    }
    fn visit_pending_iox2_events(
        &self,
        _visit: &mut dyn FnMut(crate::execution_log_schema::LoggedIox2Event),
    ) {
    }
    #[cfg(feature = "iceoryx2")]
    fn take_iox2_events<'a>(
        &mut self,
        _visit: &mut dyn FnMut(crate::iox2::Iox2EventRegistration<'a>),
    ) where
        Self: 'a,
    {
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
    fn finish_inputs(&mut self) {}
    fn flush_outputs(&mut self, _timestamp: FrameworkTime) {}
    fn discard_outputs(&mut self) {}
}

/// Update inputs, check required inputs, execute, and publish successful outputs.
/// Returns false when required inputs are missing. Error/panic paths discard
/// pending outputs. Publications from earlier callbacks remain committed.
///
/// Replay harnesses may instead drive [`prepare_callback`] and the phase handles
/// directly to inspect snapshots between phases.
pub fn execute_callback(callback: &mut dyn Callback, context: &Context) -> Result<bool, LoanError> {
    let Some(prepared) = prepare_callback(callback) else {
        return Ok(false);
    };
    prepared.run(context)?.commit();
    Ok(true)
}

pub struct Invocation<'a> {
    callback: &'a mut dyn Callback,
    ran: bool,
}

impl<'a> std::fmt::Debug for Invocation<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invocation")
            .field("ran", &self.ran)
            .finish()
    }
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
#[derive(Debug)]
#[must_use]
pub struct PreparedCallback<'a>(Invocation<'a>);

/// Completed body with unpublished outputs. Drop cancels; commit publishes at
/// the invocation timestamp. The callback cannot be reused while this exists.
#[derive(Debug)]
#[must_use]
pub struct CompletedCallback<'a> {
    invocation: Invocation<'a>,
    timestamp: FrameworkTime,
}

pub fn prepare_callback(callback: &mut dyn Callback) -> Option<PreparedCallback<'_>> {
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

impl PreparedCallback<'_> {
    pub fn visit_prepared_messages(&self, visit: &mut dyn FnMut(LoggedMessage)) {
        self.0.callback.visit_prepared_messages(visit);
    }
    pub fn visit_prepared_iox2_events(&self, visit: &mut dyn FnMut(LoggedIox2Event)) {
        self.0.callback.visit_prepared_iox2_events(visit);
    }
    pub fn required_inputs_ready(&self) -> bool {
        self.0.callback.required_inputs_ready()
    }
}
impl<'a> PreparedCallback<'a> {
    pub fn run(mut self, context: &Context) -> Result<CompletedCallback<'a>, LoanError> {
        self.0.ran = true;
        self.0.callback.run(context)?;
        Ok(CompletedCallback {
            invocation: self.0,
            timestamp: context.now(),
        })
    }
}
impl CompletedCallback<'_> {
    pub fn visit_pending_iox2_events(&self, visit: &mut dyn FnMut(LoggedIox2Event)) {
        self.invocation.callback.visit_pending_iox2_events(visit);
    }
    pub fn commit(self) {
        self.invocation.callback.flush_outputs(self.timestamp);
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::testing::FakeCallback;
    use googletest::prelude::*;

    #[gtest]
    fn callback_transition() {
        let mut fake_callback = FakeCallback {
            required_inputs_ready: true,
            ..Default::default()
        };

        let prepared_callback = {
            let prepared_callback = prepare_callback(&mut fake_callback);
            assert_true!(prepared_callback.is_some());
            prepared_callback.unwrap()
        };

        let completed_callback = {
            let context_builder = crate::context::testing::ContextBuilder {
                now: FrameworkTime::from_nanos(42),
                ..Default::default()
            };

            let completed_callback = prepared_callback.run(&context_builder.build());

            assert_true!(completed_callback.is_ok());

            completed_callback.unwrap()
        };
        expect_that!(
            completed_callback.timestamp,
            eq(FrameworkTime::from_nanos(42))
        );

        let invocation = &completed_callback.invocation;
        expect_true!(invocation.ran);

        completed_callback.commit();

        expect_that!(
            fake_callback.output_flush_timestamps,
            container_eq(vec![FrameworkTime::from_nanos(42)]),
            "One a callback has been committed, we should see the flushed outputs that we queued up"
        );
    }
}
