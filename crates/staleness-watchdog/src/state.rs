//! The pure, time-injected watchdog state machine.

use std::collections::VecDeque;
use std::time::Duration;

use crate::config::{ConfigError, WatchdogConfig};

/// `NodeId` of the standard OPC UA variable `Server_ServerStatus_CurrentTime`
/// (namespace 0, numeric id 2258). Public OPC UA specification identifier.
pub const SERVER_STATUS_CURRENT_TIME: &str = "ns=0;i=2258";

/// A server-time sample as read from the probed node, in OPC UA `DateTime` units
/// (100-nanosecond ticks). The watchdog only compares samples for equality; it never
/// interprets them as wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerTime(pub i64);

/// Result of one probe as seen by the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The probe returned a value.
    Value(ServerTime),
    /// The probe errored or timed out. Treated exactly like "the value did not change".
    Failed,
}

/// What the watchdog concluded from one probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The value changed since the previous probe.
    Healthy,
    /// The value has not changed for `age`, which is still within the stale threshold.
    Stale {
        /// Time since the freshness baseline (last change or last reconnect).
        age: Duration,
    },
    /// The value has not changed for longer than the threshold: reconnect now.
    Reconnect {
        /// Time since the freshness baseline when the reconnect was decided.
        age: Duration,
    },
    /// A reconnect is warranted, but the previous one was less than the cooldown ago.
    SuppressedByCooldown {
        /// Time since the freshness baseline.
        age: Duration,
        /// Time until the cooldown expires.
        remaining: Duration,
    },
    /// A reconnect is warranted, but the burst cap for the rolling window is exhausted.
    SuppressedByBurstCap {
        /// Time since the freshness baseline.
        age: Duration,
        /// Time until the oldest reconnect leaves the rolling window.
        retry_after: Duration,
    },
}

impl Decision {
    /// `true` only for [`Decision::Reconnect`].
    #[must_use]
    pub fn should_reconnect(&self) -> bool {
        matches!(self, Decision::Reconnect { .. })
    }

    /// Short stable label, useful for logs and metrics. Labels are compile-time literals,
    /// hence `'static`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Decision::Healthy => "healthy",
            Decision::Stale { .. } => "stale",
            Decision::Reconnect { .. } => "reconnect",
            Decision::SuppressedByCooldown { .. } => "suppressed-cooldown",
            Decision::SuppressedByBurstCap { .. } => "suppressed-burst-cap",
        }
    }
}

/// Deterministic staleness watchdog.
///
/// Invariants:
/// * `now` passed to [`Watchdog::observe`] is monotonically non-decreasing. A smaller
///   `now` is tolerated (all subtractions saturate) but yields an age of zero.
/// * `recent_reconnects` never holds more than `config.burst_cap` entries: an entry is
///   pushed only after pruning and only while `len < burst_cap`.
///
/// Boundary choices (documented, and pinned by tests):
/// * stall: `age > stale_threshold` (exactly 90 s is still `Stale`);
/// * cooldown: a reconnect is allowed once `now - last_reconnect >= reconnect_cooldown`;
/// * window: a reconnect at time `t` stops counting once `now - t >= burst_window`.
#[derive(Debug, Clone)]
pub struct Watchdog {
    config: WatchdogConfig,
    last_value: Option<ServerTime>,
    /// Time of the last value change or last reconnect, whichever is later.
    baseline: Duration,
    last_reconnect: Option<Duration>,
    recent_reconnects: VecDeque<Duration>,
}

impl Watchdog {
    /// Create a watchdog whose freshness baseline is `now`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if `config` fails [`WatchdogConfig::validate`].
    pub fn new(config: WatchdogConfig, now: Duration) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            last_value: None,
            baseline: now,
            last_reconnect: None,
            recent_reconnects: VecDeque::with_capacity(config.burst_cap),
        })
    }

    /// The validated configuration.
    #[must_use]
    pub fn config(&self) -> &WatchdogConfig {
        &self.config
    }

    /// Feed one probe result observed at `now` and get the decision for it.
    ///
    /// Any *change* of the value counts as liveness, including a backwards step (a server
    /// clock correction must not look like a stall). A [`Decision::Reconnect`] records the
    /// attempt against cooldown and burst cap and resets the freshness baseline to `now`,
    /// whether or not the caller's reconnect later succeeds.
    pub fn observe(&mut self, now: Duration, outcome: ProbeOutcome) -> Decision {
        if let ProbeOutcome::Value(value) = outcome {
            if self.last_value != Some(value) {
                self.last_value = Some(value);
                self.baseline = now;
                return Decision::Healthy;
            }
        }

        let age = now.saturating_sub(self.baseline);
        if age <= self.config.stale_threshold {
            return Decision::Stale { age };
        }

        self.prune_window(now);

        if let Some(last) = self.last_reconnect {
            let since = now.saturating_sub(last);
            if since < self.config.reconnect_cooldown {
                return Decision::SuppressedByCooldown {
                    age,
                    remaining: self.config.reconnect_cooldown - since,
                };
            }
        }

        if self.recent_reconnects.len() >= self.config.burst_cap {
            // burst_cap >= 1 (validated), so the deque is non-empty here.
            let oldest = self.recent_reconnects.front().copied().unwrap_or(now);
            let retry_after = (oldest + self.config.burst_window).saturating_sub(now);
            return Decision::SuppressedByBurstCap { age, retry_after };
        }

        self.recent_reconnects.push_back(now);
        self.last_reconnect = Some(now);
        self.baseline = now;
        Decision::Reconnect { age }
    }

    /// Number of reconnects still inside the rolling window as of `now`.
    #[must_use]
    pub fn reconnects_in_window(&self, now: Duration) -> usize {
        self.recent_reconnects
            .iter()
            .filter(|&&t| now.saturating_sub(t) < self.config.burst_window)
            .count()
    }

    fn prune_window(&mut self, now: Duration) {
        while let Some(&t) = self.recent_reconnects.front() {
            if now.saturating_sub(t) >= self.config.burst_window {
                self.recent_reconnects.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn dog() -> Watchdog {
        Watchdog::new(WatchdogConfig::default(), Duration::ZERO).unwrap()
    }

    /// Probe every 30 s from t = 0 to `until_s` inclusive with the scripted outcome for each
    /// tick; returns `(t_seconds, decision)` pairs.
    fn drive(
        dog: &mut Watchdog,
        until_s: u64,
        mut script: impl FnMut(u64) -> ProbeOutcome,
    ) -> Vec<(u64, Decision)> {
        (0..=until_s)
            .step_by(30)
            .map(|t| (t, dog.observe(secs(t), script(t))))
            .collect()
    }

    fn frozen(_: u64) -> ProbeOutcome {
        ProbeOutcome::Value(ServerTime(1_000))
    }

    fn reconnect_times(log: &[(u64, Decision)]) -> Vec<u64> {
        log.iter()
            .filter(|(_, d)| d.should_reconnect())
            .map(|(t, _)| *t)
            .collect()
    }

    #[test]
    fn healthy_advancing_stream_never_reconnects() {
        let mut d = dog();
        let log = drive(&mut d, 4 * 3600, |t| {
            ProbeOutcome::Value(ServerTime(i64::try_from(t).unwrap()))
        });
        assert!(log.iter().all(|(_, d)| *d == Decision::Healthy));
    }

    #[test]
    fn frozen_value_reconnects_only_after_threshold() {
        let mut d = dog();
        let log = drive(&mut d, 120, frozen);
        assert_eq!(
            log,
            vec![
                (0, Decision::Healthy),
                (30, Decision::Stale { age: secs(30) }),
                (60, Decision::Stale { age: secs(60) }),
                // Boundary: exactly at the threshold is still Stale.
                (90, Decision::Stale { age: secs(90) }),
                (120, Decision::Reconnect { age: secs(120) }),
            ]
        );
    }

    #[test]
    fn boundary_is_strictly_greater_than_threshold() {
        let mut d = dog();
        let v = ProbeOutcome::Value(ServerTime(7));
        assert_eq!(d.observe(Duration::ZERO, v), Decision::Healthy);
        assert_eq!(d.observe(secs(90), v), Decision::Stale { age: secs(90) });
        let just_after = secs(90) + Duration::from_nanos(1);
        assert_eq!(
            d.observe(just_after, v),
            Decision::Reconnect { age: just_after }
        );
    }

    #[test]
    fn cooldown_suppresses_second_reconnect() {
        let mut d = dog();
        let log = drive(&mut d, 420, frozen);
        assert_eq!(reconnect_times(&log), vec![120, 420]);
        // Reconnect at 120 resets the baseline; 150..=210 are Stale again (age <= 90).
        assert!(log.contains(&(210, Decision::Stale { age: secs(90) })));
        // At 240 a reconnect is warranted (age 120) but only 120 s of cooldown have passed.
        assert!(log.contains(&(
            240,
            Decision::SuppressedByCooldown {
                age: secs(120),
                remaining: secs(180),
            }
        )));
        assert!(log.contains(&(
            390,
            Decision::SuppressedByCooldown {
                age: secs(270),
                remaining: secs(30),
            }
        )));
    }

    #[test]
    fn fifth_reconnect_in_rolling_hour_is_suppressed_until_window_slides() {
        let mut d = dog();
        let log = drive(&mut d, 4000, frozen);
        assert_eq!(reconnect_times(&log), vec![120, 420, 720, 1020, 3720]);
        // 1320: cooldown since 1020 has elapsed, but four reconnects are in the window.
        assert!(log.contains(&(
            1320,
            Decision::SuppressedByBurstCap {
                age: secs(300),
                retry_after: secs(2400),
            }
        )));
        // 3690: one tick before the oldest (t=120) leaves the window.
        assert!(log.contains(&(
            3690,
            Decision::SuppressedByBurstCap {
                age: secs(2670),
                retry_after: secs(30),
            }
        )));
        assert_eq!(d.reconnects_in_window(secs(3720)), 4);
    }

    #[test]
    fn recovery_after_reconnect_returns_to_healthy() {
        let mut d = dog();
        let log = drive(&mut d, 300, |t| {
            if t <= 120 {
                ProbeOutcome::Value(ServerTime(1))
            } else {
                ProbeOutcome::Value(ServerTime(i64::try_from(t).unwrap()))
            }
        });
        assert_eq!(reconnect_times(&log), vec![120]);
        assert!(log
            .iter()
            .filter(|(t, _)| *t > 120)
            .all(|(_, d)| *d == Decision::Healthy));
    }

    #[test]
    fn probe_errors_count_as_stale() {
        let mut d = dog();
        let log = drive(&mut d, 120, |_| ProbeOutcome::Failed);
        assert_eq!(
            log[0],
            (
                0,
                Decision::Stale {
                    age: Duration::ZERO
                }
            )
        );
        assert_eq!(log[3], (90, Decision::Stale { age: secs(90) }));
        assert_eq!(reconnect_times(&log), vec![120]);
    }

    #[test]
    fn failures_after_good_data_age_from_last_change() {
        let mut d = dog();
        let log = drive(&mut d, 300, |t| {
            if t <= 60 {
                ProbeOutcome::Value(ServerTime(i64::try_from(t).unwrap()))
            } else {
                ProbeOutcome::Failed
            }
        });
        // Last change at 60, so 150 is age 90 (Stale) and 180 is age 120 (Reconnect).
        assert!(log.contains(&(150, Decision::Stale { age: secs(90) })));
        assert_eq!(reconnect_times(&log), vec![180]);
    }

    #[test]
    fn backwards_clock_step_counts_as_liveness() {
        let mut d = dog();
        assert_eq!(
            d.observe(secs(0), ProbeOutcome::Value(ServerTime(500))),
            Decision::Healthy
        );
        assert_eq!(
            d.observe(secs(30), ProbeOutcome::Value(ServerTime(100))),
            Decision::Healthy
        );
    }

    #[test]
    fn invalid_config_is_rejected_by_constructor() {
        let bad = WatchdogConfig {
            burst_cap: 0,
            ..WatchdogConfig::default()
        };
        assert_eq!(
            Watchdog::new(bad, Duration::ZERO).unwrap_err(),
            ConfigError::ZeroBurstCap
        );
    }
}
