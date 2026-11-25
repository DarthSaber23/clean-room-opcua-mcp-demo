//! Deterministic OEE arithmetic: `OEE = availability x performance x quality`.
//!
//! Computed in Rust from raw counters, never estimated by a language model. Aggregation
//! over several machines sums the underlying counters first (time-weighted), rather than
//! averaging per-machine percentages, which would over-weight short shifts.

use serde::Serialize;
use thiserror::Error;

use crate::catalog::ShiftCounters;

/// Summed counters over one or more machines.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Totals {
    /// Planned production time, seconds.
    pub planned_time_s: u64,
    /// Run time, seconds.
    pub run_time_s: u64,
    /// Ideal time for the units produced, milliseconds (sum of `ideal_cycle_ms * total`).
    pub ideal_time_ms: u64,
    /// Units produced.
    pub total_count: u64,
    /// Good units.
    pub good_count: u64,
}

impl Totals {
    /// Totals for a single machine's counters.
    #[must_use]
    pub fn from_counters(c: &ShiftCounters) -> Self {
        Self {
            planned_time_s: c.planned_time_s,
            run_time_s: c.run_time_s,
            ideal_time_ms: c.ideal_cycle_time_ms.saturating_mul(c.total_count),
            total_count: c.total_count,
            good_count: c.good_count,
        }
    }

    /// Component-wise saturating sum.
    #[must_use]
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            planned_time_s: self.planned_time_s.saturating_add(other.planned_time_s),
            run_time_s: self.run_time_s.saturating_add(other.run_time_s),
            ideal_time_ms: self.ideal_time_ms.saturating_add(other.ideal_time_ms),
            total_count: self.total_count.saturating_add(other.total_count),
            good_count: self.good_count.saturating_add(other.good_count),
        }
    }
}

/// OEE and its three factors, each as a fraction (1.0 = 100 %).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Oee {
    /// `run_time / planned_time`.
    pub availability: f64,
    /// `ideal_time / run_time`. Not clamped: a value above 1.0 signals a mis-set ideal
    /// cycle time, which should be visible rather than hidden.
    pub performance: f64,
    /// `good / total`.
    pub quality: f64,
    /// `availability * performance * quality`.
    pub oee: f64,
}

/// Counters that cannot produce a meaningful OEE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OeeError {
    /// No planned production time: OEE is undefined.
    #[error("planned production time is zero")]
    NoPlannedTime,
    /// Run time exceeds planned time.
    #[error("run time exceeds planned time")]
    RunExceedsPlanned,
    /// Good count exceeds total count.
    #[error("good count exceeds total count")]
    GoodExceedsTotal,
}

#[allow(clippy::cast_precision_loss)] // Counters are far below 2^53, so the conversion is exact.
fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

/// Compute OEE from (possibly aggregated) totals.
///
/// Zero run time gives performance 0; zero total count gives quality 0. Both make OEE 0,
/// which is the correct reading for a machine that did not produce.
///
/// # Errors
///
/// [`OeeError`] when the counters are inconsistent or planned time is zero.
pub fn compute(t: &Totals) -> Result<Oee, OeeError> {
    if t.planned_time_s == 0 {
        return Err(OeeError::NoPlannedTime);
    }
    if t.run_time_s > t.planned_time_s {
        return Err(OeeError::RunExceedsPlanned);
    }
    if t.good_count > t.total_count {
        return Err(OeeError::GoodExceedsTotal);
    }
    let availability = ratio(t.run_time_s, t.planned_time_s);
    let performance = ratio(t.ideal_time_ms, t.run_time_s.saturating_mul(1000));
    let quality = ratio(t.good_count, t.total_count);
    Ok(Oee {
        availability,
        performance,
        quality,
        oee: availability * performance * quality,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-12;

    fn counters(p: u64, r: u64, cyc: u64, tot: u64, good: u64) -> Totals {
        Totals::from_counters(&ShiftCounters {
            planned_time_s: p,
            run_time_s: r,
            ideal_cycle_time_ms: cyc,
            total_count: tot,
            good_count: good,
        })
    }

    #[test]
    fn known_numbers() {
        // 3000/3600 = 5/6; 1350 * 2 s = 2700 s of 3000 s = 0.9; 1215/1350 = 0.9.
        let o = compute(&counters(3_600, 3_000, 2_000, 1_350, 1_215)).unwrap();
        assert!((o.availability - 5.0 / 6.0).abs() < EPS);
        assert!((o.performance - 0.9).abs() < EPS);
        assert!((o.quality - 0.9).abs() < EPS);
        assert!((o.oee - 0.675).abs() < EPS);
    }

    #[test]
    fn idle_machine_is_zero_not_error() {
        let o = compute(&counters(28_800, 0, 1_200, 0, 0)).unwrap();
        assert!(o.oee.abs() < EPS);
    }

    #[test]
    fn aggregation_is_counter_weighted() {
        let a = counters(3_600, 3_600, 1_000, 3_600, 3_600); // 100 %
        let b = counters(3_600, 0, 1_000, 0, 0); // 0 %
        let o = compute(&a.saturating_add(b)).unwrap();
        assert!((o.availability - 0.5).abs() < EPS);
        assert!((o.performance - 1.0).abs() < EPS);
        assert!((o.quality - 1.0).abs() < EPS);
        assert!((o.oee - 0.5).abs() < EPS);
    }

    #[test]
    fn inconsistent_counters_are_rejected() {
        assert_eq!(
            compute(&counters(0, 0, 1, 0, 0)),
            Err(OeeError::NoPlannedTime)
        );
        assert_eq!(
            compute(&counters(10, 11, 1, 0, 0)),
            Err(OeeError::RunExceedsPlanned)
        );
        assert_eq!(
            compute(&counters(10, 10, 1, 5, 6)),
            Err(OeeError::GoodExceedsTotal)
        );
    }
}
