//! # staleness-watchdog
//!
//! A clean-room, synthetic illustration of an OPC UA *subscription staleness* watchdog.
//!
//! ## The failure mode
//!
//! An OPC UA client session can look perfectly healthy (the socket is open, keep-alives
//! succeed, the session state says "connected") while the data subscription has silently
//! stopped delivering. The cheap, server-agnostic way to detect this is to periodically
//! read a value that the server is *obliged* to keep advancing, such as the standard node
//! `Server_ServerStatus_CurrentTime` (`ns=0;i=2258`), and alarm when it stops changing.
//!
//! ## Design
//!
//! * [`Watchdog`] is a **pure, time-injected** state machine. It never reads a clock; every
//!   call takes `now` (a [`Duration`](std::time::Duration) since the watchdog started), so it
//!   is fully deterministic and unit-testable.
//! * [`run`] / [`run_with_observer`] are a thin Tokio driver around it: an interval ticker,
//!   a bounded-time probe through the [`ServerTimeSource`] trait, and a bounded-time
//!   "reconnect in place" through the [`Reconnector`] trait.
//!
//! Defaults: probe every 30 s, stale after more than 90 s without the value changing,
//! at most one reconnect per 300 s cooldown, and at most 4 reconnects per rolling 3600 s.
#![forbid(unsafe_code)]

mod config;
mod driver;
mod state;

pub use config::{ConfigError, WatchdogConfig};
pub use driver::{
    run, run_with_observer, ProbeError, ReconnectError, Reconnector, RunSummary, ServerTimeSource,
};
pub use state::{Decision, ProbeOutcome, ServerTime, Watchdog, SERVER_STATUS_CURRENT_TIME};
