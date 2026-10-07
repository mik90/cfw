use crate::{ThreadFailure, pool_state::Scheduler};
use crossbeam::channel::Sender;
use iceoryx2::prelude::*;
use std::time::Duration;
use task::iox2::{EventRecord, Iox2EventRegistration, Iox2Shutdown};

pub(crate) fn run(
    registrations: Vec<Iox2EventRegistration>,
    shutdown: Iox2Shutdown,
    scheduler: &Scheduler,
    ready: Sender<Result<(), String>>,
) -> Result<(), ThreadFailure> {
    let waitset = match WaitSetBuilder::new()
        .signal_handling_mode(SignalHandlingMode::Disabled)
        .create::<ipc_threadsafe::Service>()
    {
        Ok(waitset) => waitset,
        Err(error) => return setup_failed(&ready, error),
    };
    let mut guards = Vec::with_capacity(registrations.len());
    for registration in &registrations {
        match waitset.attach_notification(&registration.listener) {
            Ok(guard) => guards.push(guard),
            Err(error) => return setup_failed(&ready, error),
        }
    }
    let shutdown_guard = match waitset.attach_notification(&shutdown.listener) {
        Ok(guard) => guard,
        Err(error) => return setup_failed(&ready, error),
    };
    let _ = ready.send(Ok(()));
    while !scheduler.is_stopped() {
        let mut failure = None;
        let result = waitset.wait_and_process_once_with_timeout(
            |attachment| {
                if attachment.has_event_from(&shutdown_guard) {
                    return CallbackProgression::Stop;
                }
                for (registration, guard) in registrations.iter().zip(&guards) {
                    if !attachment.has_event_from(guard) {
                        continue;
                    }
                    let mut observed = false;
                    if let Err(error) = registration.listener.try_wait(|event| {
                        registration.staging.push(EventRecord {
                            event_id: event.id,
                            count: event.count,
                        });
                        observed = true;
                    }) {
                        failure = Some(error.to_string());
                        return CallbackProgression::Stop;
                    }
                    if observed {
                        registration.wake.wake();
                    }
                }
                CallbackProgression::Continue
                // The shutdown notification normally wakes immediately. The timeout
                // bounds joining even if a transport-level notification fails.
            },
            Duration::from_millis(100),
        );
        result.map_err(|error| ThreadFailure::Readiness {
            reason: error.to_string(),
        })?;
        if let Some(reason) = failure {
            return Err(ThreadFailure::Readiness { reason });
        }
    }
    Ok(())
}

fn setup_failed(
    ready: &Sender<Result<(), String>>,
    error: impl std::fmt::Display,
) -> Result<(), ThreadFailure> {
    let reason = error.to_string();
    let _ = ready.send(Err(reason.clone()));
    Err(ThreadFailure::Readiness { reason })
}
