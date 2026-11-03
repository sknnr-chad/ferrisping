//! FerrisPing's engine: ICMP probing, scheduling, per-host history, traceroute, the
//! networks (ISPs) along the way, and alerts.
//!
//! The GUI reads everything through [`Monitor`]; nothing here depends on the UI toolkit.

mod alerts;
mod engine;
mod exit;
mod history;
mod icmp;
mod network;
mod target;
mod trace;

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub use alerts::{Alert, AlertSettings, HostAlerts, Limits};
pub use engine::{Event, EventKind, Host, HostId, HostStatus, Monitor, ProbeSettings, State, StateGuard};
pub use exit::{CHECK_EVERY as EXIT_CHECK_EVERY, Exit, ExitChange};
pub use history::{History, Outcome, Sample, WindowStats};
pub use icmp::{EchoReply, EchoStatus, echo};
pub use network::{NetworkInfo, Networks, Owner, is_public};
pub use target::{Family, TargetSpec, parse_targets};
pub use trace::{Hop, HopAddr, RECENT_RESULTS, Route, Trace, TraceSettings};

/// Seconds since the UNIX epoch: the time base for every sample and event.
///
/// Anchored to the wall clock once, then advanced with the monotonic clock, so
/// samples stay in order even if Windows adjusts the system time mid-session.
pub fn now_secs() -> f64 {
    static ANCHOR: OnceLock<(f64, Instant)> = OnceLock::new();
    let (wall, mono) = ANCHOR.get_or_init(|| {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        (wall, Instant::now())
    });
    wall + mono.elapsed().as_secs_f64()
}
