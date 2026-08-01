//! Accuracy report for an exact replay run.
//!
//! The report records, per channel, how every message reference was resolved
//! and how many output comparisons diverged. It answers two questions:
//!
//! - **What was verified?** [`ReplayReport::logged_count`] counts references
//!   whose payload came from the ordinary log and was compared against the
//!   logged expected output.
//! - **What was reproduced?** [`ReplayReport::reproduced_count`] counts
//!   references whose payload was not logged and had to be reproduced by
//!   re-running the producing node.
//!
//! [`ReplayReport::exact_reproduction_ratio`] is the fraction of message
//! references that were reproduced without any gap or output mismatch, and
//! [`ReplayReport::is_exact`] is true when the whole recorded computation was
//! reproduced exactly.

use std::collections::HashMap;

use task::pub_sub::ChannelName;

/// Default cap on the number of mismatch details retained in a report. Pass a
/// different value to [`ReplayReport::new`] to override.
pub const DEFAULT_MAX_MISMATCH_DETAILS: usize = 10;

/// Per-channel tally of how message references were resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelStats {
    /// References whose payload was resolved from the ordinary log.
    pub logged: usize,
    /// References whose payload was reproduced from an unlogged channel.
    pub reproduced: usize,
    /// Output comparisons that diverged from the logged expectation.
    pub mismatches: usize,
    /// References that could not be reproduced at all.
    pub gaps: usize,
}

impl ChannelStats {
    /// Total number of message references for this channel.
    pub fn total(&self) -> usize {
        self.logged + self.reproduced + self.gaps
    }

    /// Whether every reference for this channel reproduced exactly.
    pub fn is_exact(&self) -> bool {
        self.mismatches == 0 && self.gaps == 0
    }
}

/// Snapshot of an exact replay run's accuracy. The executor owns a shared,
/// mutably-updated instance during replay; [`replay_report`] hands back a
/// clone.
///
/// [`replay_report`]: crate::ExactReplayExecutor::replay_report
#[derive(Debug, Clone, Default)]
pub struct ReplayReport {
    total_executions: usize,
    consumed_executions: usize,
    channels: HashMap<ChannelName, ChannelStats>,
    max_mismatch_details: usize,
    mismatch_details: Vec<String>,
    errors: usize,
}

impl ReplayReport {
    pub(crate) fn new(total_executions: usize, max_mismatch_details: usize) -> Self {
        ReplayReport {
            total_executions,
            consumed_executions: 0,
            channels: HashMap::new(),
            max_mismatch_details,
            mismatch_details: Vec::new(),
            errors: 0,
        }
    }

    pub(crate) fn mark_consumed(&mut self) {
        self.consumed_executions += 1;
    }

    pub(crate) fn record_logged(&mut self, channel: &str) {
        self.channels.entry(channel.to_owned()).or_default().logged += 1;
    }

    pub(crate) fn record_reproduced(&mut self, channel: &str) {
        self.channels
            .entry(channel.to_owned())
            .or_default()
            .reproduced += 1;
    }

    pub(crate) fn record_gap(&mut self, channel: &str) {
        self.channels.entry(channel.to_owned()).or_default().gaps += 1;
    }

    pub(crate) fn record_mismatch(&mut self, channel: &str, detail: String) {
        self.channels
            .entry(channel.to_owned())
            .or_default()
            .mismatches += 1;
        if self.mismatch_details.len() < self.max_mismatch_details {
            self.mismatch_details.push(detail);
        }
    }

    pub(crate) fn record_error(&mut self) {
        self.errors += 1;
    }

    /// Total number of executions parsed from the log.
    pub fn total_executions(&self) -> usize {
        self.total_executions
    }

    /// Number of executions consumed by replay so far.
    pub fn consumed_executions(&self) -> usize {
        self.consumed_executions
    }

    /// All message references across every channel.
    pub fn total_messages(&self) -> usize {
        self.channels.values().map(|stats| stats.total()).sum()
    }

    /// References whose payload was resolved from the ordinary log.
    pub fn logged_count(&self) -> usize {
        self.channels.values().map(|stats| stats.logged).sum()
    }

    /// References whose payload was reproduced from an unlogged channel.
    pub fn reproduced_count(&self) -> usize {
        self.channels.values().map(|stats| stats.reproduced).sum()
    }

    /// Output comparisons that diverged from the logged expectation.
    pub fn mismatch_count(&self) -> usize {
        self.channels.values().map(|stats| stats.mismatches).sum()
    }

    /// References that could not be reproduced at all.
    pub fn gap_count(&self) -> usize {
        self.channels.values().map(|stats| stats.gaps).sum()
    }

    /// Number of replay errors recorded (mismatches, panics, unreproducible
    /// messages, and so on).
    pub fn error_count(&self) -> usize {
        self.errors
    }

    /// Per-channel breakdown of how references were resolved.
    pub fn channel_stats(&self) -> &HashMap<ChannelName, ChannelStats> {
        &self.channels
    }

    /// Details of the first `max_mismatch_details` mismatches (configurable at
    /// construction; see [`DEFAULT_MAX_MISMATCH_DETAILS`]).
    pub fn mismatch_details(&self) -> &[String] {
        &self.mismatch_details
    }

    /// Whether the entire recorded computation was reproduced exactly: every
    /// execution consumed, no gaps, no mismatches, and no other errors.
    pub fn is_exact(&self) -> bool {
        self.consumed_executions == self.total_executions
            && self.errors == 0
            && self.channels.values().all(ChannelStats::is_exact)
    }

    /// Fraction of message references reproduced without a gap or output
    /// mismatch: `1.0` when the whole recorded computation reproduced exactly.
    /// Returns `1.0` for a report with no message references.
    pub fn exact_reproduction_ratio(&self) -> f32 {
        let total = self.total_messages();
        if total == 0 {
            return 1.0;
        }
        let exact = total - self.mismatch_count() - self.gap_count();
        exact as f32 / total as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compare a ratio to an expected value within float epsilon, avoiding
    /// exact `assert_eq!` on floats.
    fn assert_ratio_about(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < f32::EPSILON,
            "expected ratio ~{expected}, got {actual}"
        );
    }

    #[test]
    fn empty_report_is_exact() {
        let report = ReplayReport::new(0, DEFAULT_MAX_MISMATCH_DETAILS);
        assert!(report.is_exact());
        assert_ratio_about(report.exact_reproduction_ratio(), 1.0);
    }

    #[test]
    fn all_logged_no_mismatch_is_exact() {
        let mut report = ReplayReport::new(3, DEFAULT_MAX_MISMATCH_DETAILS);
        report.mark_consumed();
        report.mark_consumed();
        report.mark_consumed();
        report.record_logged("out");
        report.record_logged("out");
        assert!(report.is_exact());
        assert_ratio_about(report.exact_reproduction_ratio(), 1.0);
        assert_eq!(report.logged_count(), 2);
    }

    #[test]
    fn reproduced_channels_count_separately() {
        let mut report = ReplayReport::new(4, DEFAULT_MAX_MISMATCH_DETAILS);
        for _ in 0..4 {
            report.mark_consumed();
        }
        report.record_logged("out");
        report.record_logged("out");
        report.record_reproduced("source");
        report.record_reproduced("source");
        assert!(report.is_exact());
        assert_eq!(report.reproduced_count(), 2);
        assert_ratio_about(report.exact_reproduction_ratio(), 1.0);
    }

    #[test]
    fn mismatch_lowers_ratio_and_exactness() {
        let mut report = ReplayReport::new(1, DEFAULT_MAX_MISMATCH_DETAILS);
        report.mark_consumed();
        report.record_logged("out");
        report.record_mismatch("out", "output 0: body mismatch".to_owned());
        assert!(!report.is_exact());
        assert_eq!(report.mismatch_count(), 1);
        assert_ratio_about(report.exact_reproduction_ratio(), 0.0);
        assert_eq!(report.mismatch_details().len(), 1);
    }

    #[test]
    fn gap_lowers_ratio() {
        let mut report = ReplayReport::new(1, DEFAULT_MAX_MISMATCH_DETAILS);
        report.mark_consumed();
        report.record_logged("out");
        report.record_gap("source");
        assert!(!report.is_exact());
        assert_ratio_about(report.exact_reproduction_ratio(), 0.5);
    }

    #[test]
    fn unconsumed_executions_are_not_exact() {
        let report = ReplayReport::new(5, DEFAULT_MAX_MISMATCH_DETAILS);
        assert!(!report.is_exact());

        assert_eq!(report.consumed_executions(), 0);
        assert_eq!(report.total_executions(), 5);
    }

    #[test]
    fn mismatch_details_are_capped() {
        let mut report = ReplayReport::new(1, 2);
        report.mark_consumed();
        for i in 0..5 {
            report.record_mismatch("out", format!("mismatch {i}"));
        }
        assert_eq!(report.mismatch_count(), 5, "counts are not capped");
        assert_eq!(report.mismatch_details().len(), 2, "details are capped");
    }
}
