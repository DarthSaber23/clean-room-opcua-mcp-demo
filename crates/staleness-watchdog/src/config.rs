//! Watchdog tunables and their validation.

use std::time::Duration;

use thiserror::Error;

/// Timing policy for the watchdog. Build with [`WatchdogConfig::default`] and override
/// fields as needed; [`WatchdogConfig::validate`] is called by every constructor that
/// consumes a config, so an invalid value can never reach the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogConfig {
    /// How often the server-time value is probed. Default 30 s.
    pub probe_interval: Duration,
    /// Upper bound on a single probe or a single reconnect attempt. A probe that exceeds it
    /// is treated as a failed probe. Must be shorter than `probe_interval`. Default 10 s.
    pub io_timeout: Duration,
    /// The value may go unchanged for at most this long. Strictly *more* than this without
    /// a change is a stall. Must be greater than `probe_interval`. Default 90 s.
    pub stale_threshold: Duration,
    /// Minimum spacing between two reconnect attempts. Default 300 s.
    pub reconnect_cooldown: Duration,
    /// Maximum number of reconnect attempts inside any rolling `burst_window`. Default 4.
    pub burst_cap: usize,
    /// Width of the rolling window that `burst_cap` applies to. Default 3600 s.
    pub burst_window: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            probe_interval: Duration::from_secs(30),
            io_timeout: Duration::from_secs(10),
            stale_threshold: Duration::from_secs(90),
            reconnect_cooldown: Duration::from_secs(300),
            burst_cap: 4,
            burst_window: Duration::from_secs(3600),
        }
    }
}

/// Why a [`WatchdogConfig`] was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// A duration that must be positive was zero.
    #[error("{field} must be greater than zero")]
    ZeroDuration {
        /// Name of the offending field. A compile-time literal, hence `'static`.
        field: &'static str,
    },
    /// `io_timeout` was not shorter than `probe_interval`, so probes could overlap.
    #[error(
        "io_timeout ({io_timeout:?}) must be shorter than probe_interval ({probe_interval:?})"
    )]
    TimeoutNotBelowProbe {
        /// Configured I/O timeout.
        io_timeout: Duration,
        /// Configured probe interval.
        probe_interval: Duration,
    },
    /// `stale_threshold` was not greater than `probe_interval`, so a single delayed probe
    /// could trigger a reconnect.
    #[error(
        "stale_threshold ({stale_threshold:?}) must be greater than probe_interval ({probe_interval:?})"
    )]
    StaleNotAboveProbe {
        /// Configured stale threshold.
        stale_threshold: Duration,
        /// Configured probe interval.
        probe_interval: Duration,
    },
    /// `burst_cap` was zero, which would forbid every reconnect.
    #[error("burst_cap must be at least 1")]
    ZeroBurstCap,
}

impl WatchdogConfig {
    /// Check the internal consistency of the configuration.
    ///
    /// # Errors
    ///
    /// Returns the first [`ConfigError`] found, checked in field order.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, value) in [
            ("probe_interval", self.probe_interval),
            ("io_timeout", self.io_timeout),
            ("stale_threshold", self.stale_threshold),
            ("reconnect_cooldown", self.reconnect_cooldown),
            ("burst_window", self.burst_window),
        ] {
            if value.is_zero() {
                return Err(ConfigError::ZeroDuration { field });
            }
        }
        if self.io_timeout >= self.probe_interval {
            return Err(ConfigError::TimeoutNotBelowProbe {
                io_timeout: self.io_timeout,
                probe_interval: self.probe_interval,
            });
        }
        if self.stale_threshold <= self.probe_interval {
            return Err(ConfigError::StaleNotAboveProbe {
                stale_threshold: self.stale_threshold,
                probe_interval: self.probe_interval,
            });
        }
        if self.burst_cap == 0 {
            return Err(ConfigError::ZeroBurstCap);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_match_documented_policy() {
        let c = WatchdogConfig::default();
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(c.probe_interval, Duration::from_secs(30));
        assert_eq!(c.stale_threshold, Duration::from_secs(90));
        assert_eq!(c.reconnect_cooldown, Duration::from_secs(300));
        assert_eq!(c.burst_cap, 4);
        assert_eq!(c.burst_window, Duration::from_secs(3600));
    }

    #[test]
    fn zero_durations_are_rejected() {
        let c = WatchdogConfig {
            probe_interval: Duration::ZERO,
            ..WatchdogConfig::default()
        };
        assert_eq!(
            c.validate(),
            Err(ConfigError::ZeroDuration {
                field: "probe_interval"
            })
        );
        let c = WatchdogConfig {
            burst_window: Duration::ZERO,
            ..WatchdogConfig::default()
        };
        assert_eq!(
            c.validate(),
            Err(ConfigError::ZeroDuration {
                field: "burst_window"
            })
        );
    }

    #[test]
    fn stale_must_exceed_probe() {
        let c = WatchdogConfig {
            stale_threshold: Duration::from_secs(30),
            ..WatchdogConfig::default()
        };
        assert!(matches!(
            c.validate(),
            Err(ConfigError::StaleNotAboveProbe { .. })
        ));
    }

    #[test]
    fn timeout_must_be_below_probe() {
        let c = WatchdogConfig {
            io_timeout: Duration::from_secs(30),
            ..WatchdogConfig::default()
        };
        assert!(matches!(
            c.validate(),
            Err(ConfigError::TimeoutNotBelowProbe { .. })
        ));
    }

    #[test]
    fn burst_cap_must_be_at_least_one() {
        let c = WatchdogConfig {
            burst_cap: 0,
            ..WatchdogConfig::default()
        };
        assert_eq!(c.validate(), Err(ConfigError::ZeroBurstCap));
    }
}
