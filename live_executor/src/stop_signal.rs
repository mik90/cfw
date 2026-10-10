use crate::pool_state::Scheduler;
use crossbeam::channel::Receiver;
use std::sync::Arc;
#[cfg(feature = "iceoryx2")]
pub(crate) struct EventTarget {
    pub channel: String,
    pub inject: Box<dyn Fn(task::time::FrameworkTime, usize, u64) + Send + Sync>,
}

/// May outlive the execution scope: it references scheduling metadata only.
#[derive(Clone)]
pub struct StopSignal(pub(crate) Arc<Scheduler>);

impl StopSignal {
    /// Stop generating timer work after dispatching currently due timers, then
    /// wait for queued/running work and pending IPC readiness to drain. Gated
    /// callbacks awaiting absent inputs are idle. Future external input is outside
    /// this boundary; the caller should stop its producers first.
    pub fn drain(&self, now: task::time::FrameworkTime, timeout: std::time::Duration) -> bool {
        self.0.quiesce_timers(now);
        let started = std::time::Instant::now();
        loop {
            if self.is_stopped() || started.elapsed() >= timeout {
                return false;
            }
            let generation = self.0.completion_epoch();
            if self.0.is_idle() {
                #[cfg(feature = "iceoryx2")]
                {
                    let ticket = self.0.request_poll();
                    while !self.0.polled(ticket) {
                        if self.is_stopped() || started.elapsed() >= timeout {
                            return false;
                        }
                        let _ = self
                            .0
                            .stop_rx
                            .recv_timeout(std::time::Duration::from_millis(1));
                    }
                }
                if self.0.is_idle() && generation == self.0.completion_epoch() {
                    return true;
                }
            }
            let _ = self
                .0
                .stop_rx
                .recv_timeout(std::time::Duration::from_millis(1));
        }
    }
    #[cfg(feature = "iceoryx2")]
    pub fn inject_event(
        &self,
        callback: &str,
        ordinal: usize,
        channel: &str,
        at: task::time::FrameworkTime,
        id: usize,
        count: u64,
    ) -> Result<(), String> {
        if self.is_stopped() {
            return Err("executor is stopped".into());
        }
        let target = self
            .0
            .event_targets
            .get()
            .and_then(|targets| targets.get(&(callback.into(), ordinal)))
            .ok_or_else(|| format!("unknown event recipient '{callback}' subscriber {ordinal}"))?;
        if target.channel != channel {
            return Err(format!(
                "event recipient '{callback}' expects '{}', got '{channel}'",
                target.channel
            ));
        }
        (target.inject)(at, id, count);
        Ok(())
    }
    pub fn request_stop(&self) {
        self.0.request_stop();
    }
    pub fn is_stopped(&self) -> bool {
        self.0.is_stopped()
    }
    /// Block until shutdown is requested, including a worker failure.
    pub fn wait(&self) {
        let _ = self.0.stop_rx.recv();
    }
    /// Disconnection signals shutdown to every receiver. A main-thread signal
    /// loop can select on this alongside its OS-signal/event receiver.
    pub fn receiver(&self) -> Receiver<()> {
        self.0.stop_rx.clone()
    }
}

impl task::executor::ExecutorStopSignal for StopSignal {
    fn request_stop(&self) {
        StopSignal::request_stop(self);
    }
}
