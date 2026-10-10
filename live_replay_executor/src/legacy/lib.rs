mod replay;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use live_executor::LiveExecutor;
use task::execution_log::ExecutionLogMessage;
use task::executor::{Executor, ExecutorParams, ExecutorStopSignal, TimeSource};
use task::publisher::Publisher;
use task::time::FrameworkTime;

pub use replay::{ReplayBuildStep, build_replay};

/// Log-driven time source. Advances at `speed × wall_clock` relative to
/// the first logged timestamp.
pub struct ReplayTimeSource {
    paused: Arc<AtomicBool>,
    first_log_time: FrameworkTime,
    speed: f32,
    replay_start_wall: std::sync::OnceLock<FrameworkTime>,
    frozen_at: Mutex<Option<FrameworkTime>>,
}

impl ReplayTimeSource {
    fn new(speed: f32, first_log_time: FrameworkTime, paused: Arc<AtomicBool>) -> Self {
        ReplayTimeSource {
            paused,
            first_log_time,
            speed,
            replay_start_wall: std::sync::OnceLock::new(),
            frozen_at: Mutex::new(None),
        }
    }
}

impl TimeSource for ReplayTimeSource {
    fn now(&self) -> FrameworkTime {
        let start = *self
            .replay_start_wall
            .get_or_init(FrameworkTime::from_wall_clock);

        if self.paused.load(Ordering::Acquire) {
            return self
                .frozen_at
                .lock()
                .unwrap()
                .unwrap_or(self.first_log_time);
        }

        let wall = FrameworkTime::from_wall_clock();
        let elapsed_ns = wall.to_nanoseconds().saturating_sub(start.to_nanoseconds());
        let scaled_ns = (elapsed_ns as f64 * self.speed as f64) as i64;
        let scaled = Duration::from_nanos(scaled_ns.max(0) as u64);
        let scaled_first_log_time = self.first_log_time + scaled;

        *self.frozen_at.lock().unwrap() = Some(scaled_first_log_time);
        scaled_first_log_time
    }
}

#[derive(Debug)]
pub struct LiveReplayExecutorError {
    pub panicked_thread_indices: Vec<usize>,
}

impl std::fmt::Display for LiveReplayExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "threads panicked: {:?}", self.panicked_thread_indices)
    }
}

impl std::error::Error for LiveReplayExecutorError {}

pub struct LiveReplayConfig {
    pub replay_speed: f32,
    pub first_log_time: FrameworkTime,
    pub paused: Arc<AtomicBool>,
}

pub struct LiveReplayExecutor {
    inner: LiveExecutor<ReplayTimeSource>,
    paused: Arc<AtomicBool>,
}

impl LiveReplayExecutor {
    pub fn new(params: ExecutorParams, config: LiveReplayConfig) -> Self {
        let time_source = ReplayTimeSource::new(
            config.replay_speed,
            config.first_log_time,
            config.paused.clone(),
        );
        let inner = LiveExecutor::new_multi_pool_with_time(params, time_source);
        LiveReplayExecutor {
            inner,
            paused: config.paused.clone(),
        }
    }

    pub fn new_with_execution_log(
        params: ExecutorParams,
        log_publishers: Vec<Publisher<ExecutionLogMessage>>,
        flush_period: Duration,
        config: LiveReplayConfig,
    ) -> Self {
        let time_source = ReplayTimeSource::new(
            config.replay_speed,
            config.first_log_time,
            config.paused.clone(),
        );
        let inner = LiveExecutor::new_multi_pool_with_execution_log_and_time(
            params,
            log_publishers,
            flush_period,
            time_source,
        );
        LiveReplayExecutor {
            inner,
            paused: config.paused.clone(),
        }
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
}

impl Executor for LiveReplayExecutor {
    type Error = LiveReplayExecutorError;

    fn start(&mut self) {
        self.inner.start();
    }

    fn stop(&mut self) -> Result<(), LiveReplayExecutorError> {
        self.inner.stop().map_err(|e| LiveReplayExecutorError {
            panicked_thread_indices: e.panicked_thread_indices,
        })
    }

    fn stop_signal(&self) -> Arc<dyn ExecutorStopSignal> {
        self.inner.stop_signal()
    }

    fn is_running(&self) -> bool {
        self.inner.is_running()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use task::executor::TimeSource;

    use super::*;

    #[test]
    fn test_replay_time_source_basic() {
        let paused = Arc::new(AtomicBool::new(false));
        let source = ReplayTimeSource::new(
            1.0,
            FrameworkTime::from_nanoseconds(1_000_000_000),
            paused.clone(),
        );

        let t1 = source.now();
        let t2 = source.now();
        assert!(t2 >= t1);

        // With speed 1.0, the diff in log time should be roughly the diff in wall time
        let diff = t2.to_nanoseconds() - t1.to_nanoseconds();
        assert!(diff >= 0);
    }

    #[test]
    fn test_replay_time_source_paused() {
        let paused = Arc::new(AtomicBool::new(false));
        let source =
            ReplayTimeSource::new(2.0, FrameworkTime::from_nanoseconds(100), paused.clone());

        let t1 = source.now();
        paused.store(true, Ordering::Release);
        // small sleep to ensure wall clock would advance
        std::thread::sleep(std::time::Duration::from_millis(50));
        let t2 = source.now();
        // When paused, time should be frozen at the last snapshot
        assert_eq!(t1.to_nanoseconds(), t2.to_nanoseconds());
    }
}
