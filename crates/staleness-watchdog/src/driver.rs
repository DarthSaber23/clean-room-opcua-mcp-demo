//! Tokio driver: interval ticker + bounded-time probe + bounded-time reconnect-in-place.

use std::future::Future;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::watch;
use tokio::time::{interval, timeout, Instant, MissedTickBehavior};

use crate::config::{ConfigError, WatchdogConfig};
use crate::state::{Decision, ProbeOutcome, ServerTime, Watchdog};

/// A probe failed (transport error, bad status code, session not usable...).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("probe failed: {reason}")]
pub struct ProbeError {
    reason: String,
}

impl ProbeError {
    /// Wrap a human-readable reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// A reconnect-in-place attempt failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("reconnect failed: {reason}")]
pub struct ReconnectError {
    reason: String,
}

impl ReconnectError {
    /// Wrap a human-readable reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Reads the probed server-time value, typically `ns=0;i=2258`
/// ([`crate::SERVER_STATUS_CURRENT_TIME`]) on the *existing* session.
///
/// Implementations should not impose their own timeout; the driver bounds every call with
/// [`WatchdogConfig::io_timeout`] and treats expiry as a failed probe.
pub trait ServerTimeSource {
    /// Read the current server-time value once.
    fn read_server_time(&mut self) -> impl Future<Output = Result<ServerTime, ProbeError>> + Send;
}

/// Recovers a stalled subscription **in place**: re-use the existing session and recreate
/// or republish its subscription / monitored items. Implementations must not open an
/// additional session per call; repeated reconnects that leak sessions are exactly what the
/// cooldown and burst cap exist to prevent on resource-limited servers.
pub trait Reconnector {
    /// Perform one reconnect-in-place attempt.
    fn reconnect_in_place(&mut self) -> impl Future<Output = Result<(), ReconnectError>> + Send;
}

/// Counters returned when [`run`] shuts down.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RunSummary {
    /// Probes attempted.
    pub probes: u64,
    /// Probes that errored or timed out.
    pub probe_failures: u64,
    /// Decisions of kind [`Decision::Healthy`].
    pub healthy: u64,
    /// Decisions of kind [`Decision::Stale`].
    pub stale: u64,
    /// Reconnect attempts that returned `Ok` within the timeout.
    pub reconnects_succeeded: u64,
    /// Reconnect attempts that errored or timed out.
    pub reconnects_failed: u64,
    /// Decisions of kind [`Decision::SuppressedByCooldown`].
    pub suppressed_by_cooldown: u64,
    /// Decisions of kind [`Decision::SuppressedByBurstCap`].
    pub suppressed_by_burst_cap: u64,
}

impl RunSummary {
    fn record(&mut self, outcome: ProbeOutcome, decision: Decision) {
        self.probes += 1;
        if outcome == ProbeOutcome::Failed {
            self.probe_failures += 1;
        }
        match decision {
            Decision::Healthy => self.healthy += 1,
            Decision::Stale { .. } => self.stale += 1,
            // Counted after the attempt completes, as succeeded or failed.
            Decision::Reconnect { .. } => {}
            Decision::SuppressedByCooldown { .. } => self.suppressed_by_cooldown += 1,
            Decision::SuppressedByBurstCap { .. } => self.suppressed_by_burst_cap += 1,
        }
    }
}

/// Run the watchdog until `shutdown` becomes `true` or its sender is dropped.
///
/// See [`run_with_observer`] for the scheduling and shutdown contract.
///
/// # Errors
///
/// Returns [`ConfigError`] before doing any I/O if `config` is invalid.
pub async fn run<S, R>(
    source: &mut S,
    reconnector: &mut R,
    config: WatchdogConfig,
    shutdown: watch::Receiver<bool>,
) -> Result<RunSummary, ConfigError>
where
    S: ServerTimeSource,
    R: Reconnector,
{
    run_with_observer(source, reconnector, config, shutdown, |_, _| {}).await
}

/// Like [`run`], additionally calling `observer(elapsed, decision)` after every probe.
///
/// Scheduling contract:
/// * The first probe happens immediately, then every `probe_interval`.
/// * [`MissedTickBehavior::Delay`]: if a probe plus reconnect overruns the interval, the next
///   probe is pushed back rather than fired in a catch-up burst. A burst of back-to-back
///   probes would hammer a server that is already struggling, and could compress several
///   "stale" observations into one instant.
/// * Probes and reconnects run sequentially in this task, each bounded by `io_timeout`, so
///   there is no internal queue: at most one operation is in flight.
///
/// Shutdown contract: `shutdown` is polled with priority (`biased`) at every wait point. It
/// is observed between operations, never mid-probe, so worst-case shutdown latency is
/// `2 * io_timeout` (one probe plus one reconnect). Dropping the sender also shuts down.
///
/// The source and reconnector are borrowed, not owned, so the caller keeps them (and their
/// session) after shutdown and no `'static` bound is needed.
///
/// # Errors
///
/// Returns [`ConfigError`] before doing any I/O if `config` is invalid.
pub async fn run_with_observer<S, R, F>(
    source: &mut S,
    reconnector: &mut R,
    config: WatchdogConfig,
    mut shutdown: watch::Receiver<bool>,
    mut observer: F,
) -> Result<RunSummary, ConfigError>
where
    S: ServerTimeSource,
    R: Reconnector,
    F: FnMut(Duration, Decision),
{
    let start = Instant::now();
    let mut watchdog = Watchdog::new(config, Duration::ZERO)?;
    let mut summary = RunSummary::default();
    let mut ticker = interval(config.probe_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            // Resolves when the value is (or becomes) true, or with Err if the sender is gone.
            _ = shutdown.wait_for(|stop| *stop) => break,
            _ = ticker.tick() => {}
        }

        let outcome = match timeout(config.io_timeout, source.read_server_time()).await {
            Ok(Ok(value)) => ProbeOutcome::Value(value),
            Ok(Err(_)) | Err(_) => ProbeOutcome::Failed,
        };
        let now = start.elapsed();
        let decision = watchdog.observe(now, outcome);
        summary.record(outcome, decision);
        observer(now, decision);

        if decision.should_reconnect() {
            match timeout(config.io_timeout, reconnector.reconnect_in_place()).await {
                Ok(Ok(())) => summary.reconnects_succeeded += 1,
                Ok(Err(_)) | Err(_) => summary.reconnects_failed += 1,
            }
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the same value forever: a silently stalled subscription.
    struct FrozenSource;

    impl ServerTimeSource for FrozenSource {
        async fn read_server_time(&mut self) -> Result<ServerTime, ProbeError> {
            Ok(ServerTime(42))
        }
    }

    /// Never answers: exercises the probe timeout.
    struct HangingSource;

    impl ServerTimeSource for HangingSource {
        async fn read_server_time(&mut self) -> Result<ServerTime, ProbeError> {
            std::future::pending().await
        }
    }

    /// Records the virtual time of each reconnect attempt.
    struct RecordingReconnector {
        start: Instant,
        calls: Vec<Duration>,
        fail: bool,
    }

    impl RecordingReconnector {
        fn new(fail: bool) -> Self {
            Self {
                start: Instant::now(),
                calls: Vec::new(),
                fail,
            }
        }
    }

    impl Reconnector for RecordingReconnector {
        async fn reconnect_in_place(&mut self) -> Result<(), ReconnectError> {
            self.calls.push(self.start.elapsed());
            if self.fail {
                Err(ReconnectError::new("synthetic failure"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stall_triggers_one_reconnect_driven_by_manual_advance() {
        let (tx, rx) = watch::channel(false);
        let mut source = FrozenSource;
        let mut reconnector = RecordingReconnector::new(false);

        let controller = async {
            for _ in 0..5 {
                tokio::time::advance(Duration::from_secs(30)).await;
            }
            tx.send(true).unwrap();
        };
        let (summary, ()) = tokio::join!(
            run(&mut source, &mut reconnector, WatchdogConfig::default(), rx),
            controller
        );
        let summary = summary.unwrap();

        assert_eq!(reconnector.calls, vec![Duration::from_secs(120)]);
        assert_eq!(summary.reconnects_succeeded, 1);
        assert_eq!(summary.healthy, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn frozen_source_respects_cooldown_over_virtual_time() {
        let (tx, rx) = watch::channel(false);
        let mut source = FrozenSource;
        let mut reconnector = RecordingReconnector::new(false);

        let controller = async {
            tokio::time::sleep(Duration::from_secs(1000)).await;
            tx.send(true).unwrap();
        };
        let (summary, ()) = tokio::join!(
            run(&mut source, &mut reconnector, WatchdogConfig::default(), rx),
            controller
        );
        let summary = summary.unwrap();

        assert_eq!(
            reconnector.calls,
            vec![
                Duration::from_secs(120),
                Duration::from_secs(420),
                Duration::from_secs(720)
            ]
        );
        // Ticks at 0, 30, ..., 990.
        assert_eq!(summary.probes, 34);
        assert_eq!(summary.reconnects_succeeded, 3);
        assert!(summary.suppressed_by_cooldown > 0);
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_probe_times_out_and_counts_as_failure() {
        let (tx, rx) = watch::channel(false);
        let mut source = HangingSource;
        let mut reconnector = RecordingReconnector::new(true);

        let controller = async {
            tokio::time::sleep(Duration::from_secs(200)).await;
            tx.send(true).unwrap();
        };
        let (summary, ()) = tokio::join!(
            run(&mut source, &mut reconnector, WatchdogConfig::default(), rx),
            controller
        );
        let summary = summary.unwrap();

        // Probes start at 0, 30, ..., 180 and each takes the full 10 s timeout.
        assert_eq!(summary.probes, 7);
        assert_eq!(summary.probe_failures, 7);
        // Probe started at 90 completes at 100 (age 100 > 90) and triggers the reconnect.
        assert_eq!(reconnector.calls, vec![Duration::from_secs(100)]);
        assert_eq!(summary.reconnects_failed, 1);
        assert_eq!(summary.reconnects_succeeded, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_sender_shuts_down_and_invalid_config_is_rejected() {
        let (tx, rx) = watch::channel(false);
        drop(tx);
        let summary = run(
            &mut FrozenSource,
            &mut RecordingReconnector::new(false),
            WatchdogConfig::default(),
            rx,
        )
        .await
        .unwrap();
        assert_eq!(summary.probes, 0);

        let (_tx, rx) = watch::channel(false);
        let bad = WatchdogConfig {
            burst_cap: 0,
            ..WatchdogConfig::default()
        };
        let err = run(
            &mut FrozenSource,
            &mut RecordingReconnector::new(false),
            bad,
            rx,
        )
        .await
        .unwrap_err();
        assert_eq!(err, ConfigError::ZeroBurstCap);
    }
}
