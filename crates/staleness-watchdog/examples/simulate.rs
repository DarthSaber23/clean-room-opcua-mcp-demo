//! Replay synthetic subscription stalls in virtual time and print the watchdog decisions.
//!
//! Runs on a paused Tokio clock, so hours of simulated time finish in milliseconds.
//!
//! ```text
//! cargo run --example simulate -p staleness-watchdog
//! ```

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use staleness_watchdog::{
    run_with_observer, Decision, ProbeError, ReconnectError, Reconnector, ServerTime,
    ServerTimeSource, WatchdogConfig, SERVER_STATUS_CURRENT_TIME,
};
use tokio::sync::watch;
use tokio::time::Instant;

/// A synthetic OPC UA server whose `CurrentTime` freezes at `stall_from` while the session
/// keeps looking connected. The stall clears after `clears_after_reconnects` reconnects.
///
/// Shared by the probe source and the reconnector through plain `&` references; the only
/// mutable state is an atomic counter, so no `Arc` or `Mutex` is required.
struct SimServer {
    start: Instant,
    stall_from: Duration,
    clears_after_reconnects: u32,
    reconnects: AtomicU32,
}

impl SimServer {
    fn current_time(&self) -> ServerTime {
        let elapsed = self.start.elapsed();
        let stalled = elapsed >= self.stall_from
            && self.reconnects.load(Ordering::Relaxed) < self.clears_after_reconnects;
        let effective = if stalled { self.stall_from } else { elapsed };
        // 100 ns OPC UA ticks; saturate rather than wrap on absurd durations.
        ServerTime(i64::try_from(effective.as_nanos() / 100).unwrap_or(i64::MAX))
    }
}

struct SimSource<'a> {
    server: &'a SimServer,
}

impl ServerTimeSource for SimSource<'_> {
    async fn read_server_time(&mut self) -> Result<ServerTime, ProbeError> {
        Ok(self.server.current_time())
    }
}

struct SimReconnector<'a> {
    server: &'a SimServer,
}

impl Reconnector for SimReconnector<'_> {
    async fn reconnect_in_place(&mut self) -> Result<(), ReconnectError> {
        // Re-uses the existing (synthetic) session: no new session is opened.
        self.server.reconnects.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn describe(decision: Decision) -> String {
    match decision {
        Decision::Healthy => "healthy".to_owned(),
        Decision::Stale { age } => format!("stale (no change for {}s)", age.as_secs()),
        Decision::Reconnect { age } => {
            format!("RECONNECT in place (no change for {}s)", age.as_secs())
        }
        Decision::SuppressedByCooldown { age, remaining } => format!(
            "suppressed by cooldown (no change for {}s, {}s remaining)",
            age.as_secs(),
            remaining.as_secs()
        ),
        Decision::SuppressedByBurstCap { age, retry_after } => format!(
            "suppressed by burst cap (no change for {}s, retry after {}s)",
            age.as_secs(),
            retry_after.as_secs()
        ),
    }
}

async fn scenario(
    title: &str,
    stall_from_s: u64,
    clears_after: u32,
    duration_s: u64,
    verbose: bool,
) {
    println!("\n=== {title} ===");
    println!(
        "probing {SERVER_STATUS_CURRENT_TIME}; stall from t={stall_from_s}s; virtual duration {duration_s}s"
    );
    let server = SimServer {
        start: Instant::now(),
        stall_from: Duration::from_secs(stall_from_s),
        clears_after_reconnects: clears_after,
        reconnects: AtomicU32::new(0),
    };
    let mut source = SimSource { server: &server };
    let mut reconnector = SimReconnector { server: &server };
    let (tx, rx) = watch::channel(false);

    let mut last_kind: Option<&'static str> = None;
    let observer = |t: Duration, d: Decision| {
        // In quiet mode only print when the kind of decision changes.
        if verbose || last_kind != Some(d.kind()) {
            println!("t={:>5}s  {}", t.as_secs(), describe(d));
        }
        last_kind = Some(d.kind());
    };

    let stopper = async {
        tokio::time::sleep(Duration::from_secs(duration_s)).await;
        // Err only if the receiver is gone, i.e. the watchdog already stopped.
        let _ = tx.send(true);
    };

    let (result, ()) = tokio::join!(
        run_with_observer(
            &mut source,
            &mut reconnector,
            WatchdogConfig::default(),
            rx,
            observer
        ),
        stopper
    );
    match result {
        Ok(summary) => println!("summary: {summary:?}"),
        Err(e) => println!("invalid config: {e}"),
    }
}

fn main() -> std::io::Result<()> {
    // Paused clock: timers auto-advance when the runtime is idle, so this is fast and
    // deterministic. `start_paused` is a test-util feature, enabled for examples via
    // dev-dependencies only.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()?;
    runtime.block_on(async {
        scenario(
            "silent stall at 5 min, cleared by the 2nd reconnect",
            300,
            2,
            1200,
            true,
        )
        .await;
        scenario(
            "persistent stall: cooldown then burst cap (transitions only)",
            60,
            u32::MAX,
            2 * 3600,
            false,
        )
        .await;
    });
    Ok(())
}
