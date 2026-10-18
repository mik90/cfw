use std::fmt::Debug;

use crate::time::FrameworkTime;
use crate::{Context, LoanError};
mod node;
mod ports;
pub use node::CallbackNode;
pub use ports::*;

/// Callback body and IO lifecycle, shared by live and deterministic executors.
/// Executors invoke `execute_callback` or `execute_callback_batch` rather than
/// calling `run` directly.
pub trait Callback: Send {
    /// Whether all outputs are known to avoid external transport delivery.
    fn outputs_are_local(&self) -> bool {
        false
    }
    /// Clear prepared and incoming snapshots before exact-replay hydration.
    fn clear_replay_inputs(&mut self) -> Result<(), LoanError> {
        Err(LoanError::Transport(
            "callback does not support replay input reset".into(),
        ))
    }
    fn stage_replay_event(
        &mut self,
        _ordinal: usize,
        _event_id: usize,
        _count: u64,
    ) -> Result<(), LoanError> {
        Err(LoanError::Transport("unknown replay event port".into()))
    }
    fn recording_endpoints(&self) -> Option<Vec<crate::recording::EndpointDescriptor>> {
        None
    }
    fn visit_prepared_messages(&self, _visit: &mut dyn FnMut(crate::recording::LoggedMessage)) {}
    fn visit_pending_messages(&self, _visit: &mut dyn FnMut(crate::recording::LoggedMessage)) {}
    fn visit_prepared_events(&self, _visit: &mut dyn FnMut(crate::recording::LoggedEvent)) {}
    fn visit_pending_events(&self, _visit: &mut dyn FnMut(crate::recording::LoggedEvent)) {}
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
    pub fn commit(self) {
        self.invocation.callback.flush_outputs(self.timestamp);
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use googletest::prelude::*;
    #[derive(Default)]
    struct FakeCallback {
        has_pending_inputs: bool,
        outputs_are_local: bool,
        required_inputs_available: bool,
        required_inputs_ready: bool,
        output_flush_timestamps: Vec<FrameworkTime>,
    }

    impl Callback for FakeCallback {
        fn run(&mut self, _context: &Context) -> Result<(), LoanError> {
            Ok(())
        }

        fn clear_replay_inputs(&mut self) -> Result<(), LoanError> {
            todo!()
        }

        fn discard_outputs(&mut self) {}
        fn finish_inputs(&mut self) {}
        fn flush_outputs(&mut self, timestamp: FrameworkTime) {
            self.output_flush_timestamps.push(timestamp);
        }

        fn has_pending_inputs(&self) -> bool {
            self.has_pending_inputs
        }
        fn outputs_are_local(&self) -> bool {
            self.outputs_are_local
        }

        fn recording_endpoints(&self) -> Option<Vec<crate::recording::EndpointDescriptor>> {
            None
        }

        fn required_inputs_available(&self) -> bool {
            self.required_inputs_available
        }

        fn required_inputs_ready(&self) -> bool {
            self.required_inputs_ready
        }

        fn set_waker(&mut self, _wake: crate::wake::WakeHandle) {}
        fn stage_replay_event(
            &mut self,
            _ordinal: usize,
            _event_id: usize,
            _count: u64,
        ) -> Result<(), LoanError> {
            Ok(())
        }
        #[cfg(feature = "iceoryx2")]
        fn take_iox2_events(&mut self) -> Vec<crate::iox2::Iox2EventRegistration> {
            vec![]
        }
        fn update_inputs(&mut self) {}
        fn visit_channel_names(&self, _visit: &mut dyn FnMut(&str)) {}
        fn visit_pending_events(&self, _visit: &mut dyn FnMut(crate::recording::LoggedEvent)) {}
        fn visit_pending_messages(&self, _visit: &mut dyn FnMut(crate::recording::LoggedMessage)) {}
        fn visit_prepared_events(&self, _visit: &mut dyn FnMut(crate::recording::LoggedEvent)) {}
        fn visit_prepared_messages(&self, _visit: &mut dyn FnMut(crate::recording::LoggedMessage)) {
        }
    }

    #[gtest]
    fn callback_transition() {
        let mut fake_callback = FakeCallback::default();

        fake_callback.required_inputs_ready = true;

        let prepared_callback = {
            let prepared_callback = prepare_callback(&mut fake_callback);
            assert_true!(prepared_callback.is_some());
            prepared_callback.unwrap()
        };

        let completed_callback = {
            let mut context_builder = crate::context::testing::ContextBuilder::default();
            context_builder.now = FrameworkTime::from_nanoseconds(42);

            let completed_callback = prepared_callback.run(&context_builder.build());

            assert_true!(completed_callback.is_ok());

            completed_callback.unwrap()
        };
        expect_that!(
            completed_callback.timestamp,
            eq(FrameworkTime::from_nanoseconds(42))
        );

        let invocation = &completed_callback.invocation;
        expect_true!(invocation.ran);

        completed_callback.commit();

        expect_that!(
            fake_callback.output_flush_timestamps,
            container_eq(vec![FrameworkTime::from_nanoseconds(42)]),
            "One a callback has been committed, we should see the flushed outputs that we queued up"
        );
    }
}
