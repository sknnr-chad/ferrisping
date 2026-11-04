//! Schedules probes for every host and keeps the shared state the UI reads.

use std::cell::Cell;
use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::alerts::{Alert, AlertSettings, HostAlerts, Watch};
use crate::exit::{self, Exit, ExitChange};
use crate::history::{History, Outcome, Sample};
use crate::icmp::{self, EchoReply};
use crate::network::{self, Networks};
use crate::now_secs;
use crate::paths::Paths;
use crate::target::{Family, TargetSpec};
use crate::trace::{Trace, TraceSettings};

/// How long samples are kept (a little over the longest graph window).
const RETENTION_SECS: f64 = 25.0 * 3600.0;
/// Cap per host, so very fast intervals can't use unbounded memory.
const MAX_SAMPLES_PER_HOST: usize = 400_000;
const MAX_EVENTS: usize = 2_000;
const RESOLVE_RETRY: Duration = Duration::from_secs(10);
/// Typical timer overhead on a laptop, used until the first calibration finishes.
const DEFAULT_OVERHEAD_MS: f64 = 0.5;
const CALIBRATION_PROBES: usize = 9;
const CALIBRATION_EVERY: Duration = Duration::from_secs(60);
/// Threads the engine may block in Windows calls: probes, traceroutes and lookups.
const BLOCKING_THREADS: usize = 512;
/// Most host probes out at once. Each holds a blocking thread until it's answered
/// or times out; the other threads are left for traceroutes and lookups.
const MAX_PROBES_OUT: usize = 320;
/// How many of those hosts whose last probe is still out may take. Hosts that
/// don't answer are slowed down first, so hosts that answer keep their pace.
const MAX_PROBES_OUT_WAITING: usize = 256;

/// Settings shared by every host.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeSettings {
    pub interval: Duration,
    pub timeout: Duration,
    pub payload_len: u16,
    pub ttl: u8,
    /// Consecutive lost probes before a host counts as down.
    pub down_after: u32,
    pub paused: bool,
    /// Check each host's route for the network its traffic leaves through, and ask
    /// Team Cymru who runs the public routers on any route. When this is off, neither
    /// happens: hosts have no [`Exit`], and routes look up no owners.
    pub network_lookups: bool,
}

impl Default for ProbeSettings {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(2),
            payload_len: 32,
            ttl: 128,
            down_after: 3,
            paused: false,
            network_lookups: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostStatus {
    Resolving,
    ResolveFailed,
    /// Resolved; waiting for the first answer.
    Waiting,
    Up,
    Down,
}

/// One monitored host and everything measured for it.
#[derive(Debug)]
pub struct Host {
    pub id: HostId,
    pub spec: TargetSpec,
    pub addr: Option<IpAddr>,
    pub resolve_error: Option<String>,
    pub status: HostStatus,
    /// When the current status began, in seconds since the UNIX epoch.
    pub status_since: f64,
    pub history: History,
    /// Bumped on every change, so the UI can cache what it derives from the history.
    pub version: u64,
    /// The network this host's traffic leaves through, once found.
    pub exit: Option<Exit>,
    /// Switches to another way out, oldest first.
    pub exit_changes: Vec<ExitChange>,
    /// The ways its traffic took over the last day or so, as the route checks saw them.
    pub paths: Paths,
    /// Alerts of its own, if it has them.
    pub alerts: HostAlerts,
    /// Where it stands with the loss and round-trip limits.
    watch: Watch,
    /// It was up when its latest outage began. Only then is the outage an alert: a host
    /// that never answered, like an unused address in a subnet, didn't stop answering.
    /// Only then is its route checked during the outage, too.
    pub(crate) was_up: bool,
    loss_streak: u32,
    loss_streak_start: f64,
    /// Next sample to feed into the up/down logic. Probes can finish out of
    /// order (a timeout ends after newer replies), so this walks in send order.
    next_to_process: u64,
}

impl Host {
    pub(crate) fn new(id: HostId, spec: TargetSpec, now: f64) -> Self {
        Self {
            id,
            spec,
            addr: None,
            resolve_error: None,
            status: HostStatus::Resolving,
            status_since: now,
            history: History::new(),
            version: 0,
            exit: None,
            exit_changes: Vec::new(),
            paths: Paths::default(),
            alerts: HostAlerts::default(),
            watch: Watch::default(),
            was_up: false,
            loss_streak: 0,
            loss_streak_start: now,
            next_to_process: 0,
        }
    }

    pub fn name(&self) -> &str {
        self.spec.display_name()
    }

    /// The address it's pinged at, once that's known; until then, the host as typed.
    pub fn address(&self) -> String {
        self.addr
            .map_or_else(|| self.spec.host.clone(), |addr| addr.to_string())
    }

    /// Update up/down state with one finished sample, returning an event on a change.
    fn observe(&mut self, sample: &Sample, down_after: u32) -> Option<EventKind> {
        match sample.outcome {
            Outcome::Pending => None,
            Outcome::Reply { .. } => {
                self.loss_streak = 0;
                let event = (self.status == HostStatus::Down).then_some(EventKind::Up {
                    outage_secs: sample.t - self.status_since,
                });
                if self.status != HostStatus::Up {
                    self.status = HostStatus::Up;
                    self.status_since = sample.t;
                }
                event
            }
            Outcome::Lost(_) => {
                if self.loss_streak == 0 {
                    self.loss_streak_start = sample.t;
                }
                self.loss_streak += 1;
                if self.status != HostStatus::Down && self.loss_streak >= down_after.max(1) {
                    self.was_up = self.status == HostStatus::Up;
                    self.status = HostStatus::Down;
                    self.status_since = self.loss_streak_start;
                    return Some(EventKind::Down);
                }
                None
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum EventKind {
    /// Stopped answering. The event time is the first lost probe.
    Down,
    /// Answering again after an outage.
    Up {
        outage_secs: f64,
    },
    ResolveFailed(String),
    /// Traffic leaves through another network now (AS numbers; see [`Exit`]).
    /// The event time is when the new network first showed up.
    ExitChanged {
        from: u32,
        to: u32,
    },
    /// At least the loss limit of the last pings got no reply (see [`crate::Limits`]).
    Loss {
        lost: u32,
        pings: u32,
        limit_pct: f32,
    },
    /// The loss has stayed within its limit for as many pings as it's judged over.
    LossEnded {
        limit_pct: f32,
        pings: u32,
    },
    /// The replies to the last pings averaged more than the limit.
    Slow {
        avg_ms: f32,
        pings: u32,
        limit_ms: f32,
    },
    /// Round trips have stayed within their limit for as many pings as they're judged over.
    SlowEnded {
        limit_ms: f32,
        pings: u32,
    },
}

#[derive(Clone, Debug)]
pub struct Event {
    pub at: f64,
    pub host: HostId,
    /// The host's name when this was logged. Renaming the host doesn't change it.
    pub host_name: String,
    /// The host's [address](Host::address) when this was logged.
    pub host_address: String,
    pub kind: EventKind,
}

/// Everything the UI shows. Read it with [`Monitor::read`].
#[derive(Debug)]
pub struct State {
    pub hosts: Vec<Host>,
    /// Oldest first.
    pub events: Vec<Event>,
    pub settings: ProbeSettings,
    /// Who owns the addresses seen on the way to hosts, and what the networks are called.
    pub networks: Networks,
    /// When a probe was last skipped because too many were out at once.
    pub probes_skipped_at: Option<f64>,
    /// Which events are alerts, for hosts without alerts of their own.
    pub alerts: AlertSettings,
}

impl State {
    pub fn host(&self, id: HostId) -> Option<&Host> {
        self.hosts.iter().find(|h| h.id == id)
    }

    fn host_mut(&mut self, id: HostId) -> Option<&mut Host> {
        self.hosts.iter_mut().find(|h| h.id == id)
    }

    /// The alert `event` makes, if it's one for its host.
    fn alert_for(&self, event: &Event) -> Option<Alert> {
        let host = self.host(event.host)?;
        if host.alerts == HostAlerts::Off {
            return None;
        }
        let networks = match event.kind {
            EventKind::Down | EventKind::Up { .. } if self.alerts.outages && host.was_up => None,
            EventKind::ExitChanged { from, to } if self.alerts.switches => {
                Some((self.networks.name(from), self.networks.name(to)))
            }
            // Only logged while the host has limits for them.
            EventKind::Loss { .. }
            | EventKind::LossEnded { .. }
            | EventKind::Slow { .. }
            | EventKind::SlowEnded { .. } => None,
            _ => return None,
        };
        Some(Alert {
            event: event.clone(),
            networks,
        })
    }

    pub(crate) fn push_event(&mut self, event: Event) {
        // Some events are dated back a little (an outage starts at its first lost
        // probe), so insert in time order rather than at the end.
        let at = self.events.partition_point(|e| e.at <= event.at);
        self.events.insert(at, event);
        if self.events.len() > MAX_EVENTS {
            let excess = self.events.len() - MAX_EVENTS;
            self.events.drain(..excess);
        }
    }
}

thread_local! {
    /// Whether this thread holds a read lock on the state (tracked in debug builds).
    static READING: Cell<bool> = const { Cell::new(false) };
}

/// A read lock on the [`State`]. Probes wait while it's held, so keep it short.
///
/// Don't lock the state again while holding one: the second read would wait
/// behind a probe waiting to write, and that probe waits for the first read, so
/// neither moves. Debug builds (and so the tests) stop with a panic instead.
pub struct StateGuard<'a>(RwLockReadGuard<'a, State>);

impl Deref for StateGuard<'_> {
    type Target = State;

    fn deref(&self) -> &State {
        &self.0
    }
}

impl Drop for StateGuard<'_> {
    fn drop(&mut self) {
        if cfg!(debug_assertions) {
            READING.set(false);
        }
    }
}

type ChangeFn = Box<dyn Fn() + Send + Sync>;
type AlertFn = Box<dyn Fn(Alert) + Send + Sync>;

pub(crate) struct Shared {
    state: RwLock<State>,
    pub(crate) settings_tx: watch::Sender<ProbeSettings>,
    on_change: RwLock<Option<ChangeFn>>,
    on_alert: RwLock<Option<AlertFn>>,
    /// Time our own timer adds on top of the network round trip (f64 bits, ms).
    overhead_ms: AtomicU64,
    /// Taken by each host's check of its way out, so those run one at a time.
    pub(crate) exit_turn: tokio::sync::Mutex<()>,
    /// The same for hosts that are down, which take their turns apart from the rest.
    pub(crate) down_turn: tokio::sync::Mutex<()>,
    /// Host probes out on the network right now.
    probes_out: AtomicUsize,
}

/// A probe out on the network, counted until it's answered or times out.
struct ProbeSlot {
    shared: Arc<Shared>,
    host_out: Arc<AtomicUsize>,
}

impl Drop for ProbeSlot {
    fn drop(&mut self) {
        self.shared.probes_out.fetch_sub(1, Ordering::Relaxed);
        self.host_out.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Shared {
    pub(crate) fn new(settings: ProbeSettings) -> Self {
        Self {
            state: RwLock::new(State {
                hosts: Vec::new(),
                events: Vec::new(),
                settings,
                networks: Networks::default(),
                probes_skipped_at: None,
                alerts: AlertSettings::default(),
            }),
            settings_tx: watch::Sender::new(settings),
            on_change: RwLock::new(None),
            on_alert: RwLock::new(None),
            overhead_ms: AtomicU64::new(DEFAULT_OVERHEAD_MS.to_bits()),
            exit_turn: tokio::sync::Mutex::new(()),
            down_turn: tokio::sync::Mutex::new(()),
            probes_out: AtomicUsize::new(0),
        }
    }

    /// Room for one more probe from a host that has `host_out` probes out, or None
    /// when too many are out: queued behind them, it would only fall behind.
    fn take_slot(self: &Arc<Self>, host_out: &Arc<AtomicUsize>) -> Option<ProbeSlot> {
        let limit = if host_out.load(Ordering::Relaxed) > 0 {
            MAX_PROBES_OUT_WAITING
        } else {
            MAX_PROBES_OUT
        };
        self.probes_out
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |out| {
                (out < limit).then_some(out + 1)
            })
            .ok()?;
        host_out.fetch_add(1, Ordering::Relaxed);
        Some(ProbeSlot {
            shared: Arc::clone(self),
            host_out: Arc::clone(host_out),
        })
    }

    pub(crate) fn overhead_ms(&self) -> f64 {
        f64::from_bits(self.overhead_ms.load(Ordering::Relaxed))
    }

    pub(crate) fn read(&self) -> StateGuard<'_> {
        if cfg!(debug_assertions) {
            assert!(!READING.get(), "the state is already locked for reading on this thread");
            READING.set(true);
        }
        StateGuard(self.state.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) fn write(&self) -> RwLockWriteGuard<'_, State> {
        // Waiting to write while this thread reads would wait forever.
        if cfg!(debug_assertions) {
            assert!(!READING.get(), "the state can't be written while this thread reads it");
        }
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn notify(&self) {
        if let Some(on_change) = self.on_change.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
            on_change();
        }
    }

    /// Log an event, and hand it to the alert hook if it's an alert for its host.
    /// The caller holds the write lock, which the hook knows not to take.
    pub(crate) fn log(&self, state: &mut State, event: Event) {
        if let Some(on_alert) = self.on_alert.read().unwrap_or_else(PoisonError::into_inner).as_ref()
            && let Some(alert) = state.alert_for(&event)
        {
            on_alert(alert);
        }
        state.push_event(event);
    }

    fn resolved(&self, id: HostId, addr: IpAddr) -> bool {
        let mut state = self.write();
        let Some(host) = state.host_mut(id) else {
            return false;
        };
        host.addr = Some(addr);
        host.resolve_error = None;
        host.status = HostStatus::Waiting;
        host.status_since = now_secs();
        host.version += 1;
        drop(state);
        self.notify();
        true
    }

    fn resolve_failed(&self, id: HostId, error: String) -> bool {
        let mut state = self.write();
        let Some(host) = state.host_mut(id) else {
            return false;
        };
        let first_failure = host.status != HostStatus::ResolveFailed;
        let now = now_secs();
        host.status = HostStatus::ResolveFailed;
        host.resolve_error = Some(error.clone());
        host.version += 1;
        if first_failure {
            host.status_since = now;
            let host_name = host.name().to_string();
            let host_address = host.address();
            let event = Event {
                at: now,
                host: id,
                host_name,
                host_address,
                kind: EventKind::ResolveFailed(error),
            };
            self.log(&mut state, event);
        }
        drop(state);
        self.notify();
        true
    }

    /// Record a probe as sent. Returns its sequence number, or None if the host is gone.
    fn begin_probe(&self, id: HostId, t: f64) -> Option<u64> {
        let mut state = self.write();
        let host = state.host_mut(id)?;
        let seq = host.history.push_pending(t);
        host.history
            .trim(t - RETENTION_SECS, MAX_SAMPLES_PER_HOST, host.next_to_process);
        host.paths.trim(t - RETENTION_SECS);
        host.version += 1;
        Some(seq)
    }

    fn finish_probe(&self, id: HostId, seq: u64, reply: EchoReply, down_after: u32) {
        let mut guard = self.write();
        let state = &mut *guard;
        let usual = state.alerts.limits;
        let Some(host) = state.hosts.iter_mut().find(|h| h.id == id) else {
            return;
        };
        let outcome = if reply.status.is_reply() {
            Outcome::Reply {
                rtt_ms: estimate_rtt_ms(&reply, self.overhead_ms()),
            }
        } else {
            Outcome::Lost(reply.status)
        };
        if !host.history.complete(seq, outcome) {
            return;
        }
        host.version += 1;

        let limits = host.alerts.limits(usual);
        let mut events = Vec::new();
        host.next_to_process = host.next_to_process.max(host.history.first_seq());
        while let Some(sample) = host.history.get(host.next_to_process).copied() {
            if sample.outcome == Outcome::Pending {
                break;
            }
            let done = host.next_to_process;
            host.next_to_process += 1;
            let mut kinds = Vec::new();
            if let Some(kind) = host.observe(&sample, down_after) {
                match kind {
                    // The outage is the alert now. Once it's over, its lost pings don't
                    // count toward the loss.
                    EventKind::Down => host.watch.reset(),
                    EventKind::Up { .. } => host.watch.restart(done),
                    _ => {}
                }
                kinds.push(kind);
            }
            // Loss is judged at replies, so a host that stops answering raises the
            // outage alert rather than a loss alert just before it.
            if host.status == HostStatus::Up
                && !sample.is_lost()
                && let Some(limits) = limits
            {
                kinds.extend(host.watch.judge(&host.history, done, limits));
            }
            for kind in kinds {
                let at = match kind {
                    EventKind::Down => host.status_since,
                    _ => sample.t,
                };
                events.push(Event {
                    at,
                    host: id,
                    host_name: host.name().to_string(),
                    host_address: host.address(),
                    kind,
                });
            }
        }
        for event in events {
            self.log(state, event);
        }
        drop(guard);
        self.notify();
    }
}

/// Best estimate of the round trip from one reply.
///
/// Windows reports whole milliseconds, rounded down, so the true value lies in
/// `[windows_ms, windows_ms + 1)`. Our own timer is finer but also counts the
/// time Windows takes to hand the reply back (measured against loopback), so we
/// subtract that and keep the result inside Windows' bracket.
pub(crate) fn estimate_rtt_ms(reply: &EchoReply, overhead_ms: f64) -> f32 {
    let low = reply.windows_ms as f64;
    (reply.elapsed_ms - overhead_ms).clamp(low, low + 0.999) as f32
}

/// Median of a few loopback round trips: our timer's overhead, since loopback itself takes ~0 ms.
fn measure_overhead_ms() -> Option<f64> {
    let loopback = IpAddr::from([127, 0, 0, 1]);
    let mut samples: Vec<f64> = (0..CALIBRATION_PROBES)
        .filter_map(|_| {
            let reply = icmp::echo(loopback, Duration::from_millis(500), 128, 32);
            std::thread::sleep(Duration::from_millis(10));
            reply.status.is_reply().then_some(reply.elapsed_ms)
        })
        .collect();
    if samples.len() < CALIBRATION_PROBES / 2 {
        return None;
    }
    samples.sort_by(f64::total_cmp);
    Some(samples[samples.len() / 2])
}

async fn calibrate(shared: Arc<Shared>) {
    loop {
        if let Ok(Some(overhead)) = tokio::task::spawn_blocking(measure_overhead_ms).await {
            shared.overhead_ms.store(overhead.to_bits(), Ordering::Relaxed);
        }
        tokio::time::sleep(CALIBRATION_EVERY).await;
    }
}

/// Runs the probes and owns the state the UI displays.
pub struct Monitor {
    shared: Arc<Shared>,
    runtime: Option<Runtime>,
    tasks: Mutex<HashMap<HostId, JoinHandle<()>>>,
    next_id: AtomicU64,
}

impl Monitor {
    pub fn new(settings: ProbeSettings) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(BLOCKING_THREADS)
            .thread_name("ferrisping-engine")
            .enable_time()
            .build()
            .expect("failed to start the probe runtime");
        let shared = Arc::new(Shared::new(settings));
        runtime.spawn(calibrate(shared.clone()));
        Self {
            shared,
            runtime: Some(runtime),
            tasks: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// The timer overhead currently subtracted from each round trip, in milliseconds.
    pub fn timer_overhead_ms(&self) -> f64 {
        self.shared.overhead_ms()
    }

    /// Called from engine threads whenever something changes (e.g. to request a repaint).
    pub fn set_on_change(&self, on_change: impl Fn() + Send + Sync + 'static) {
        *self.shared.on_change.write().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(on_change));
    }

    /// Called from engine threads with each alert as it's logged. The state is
    /// locked for writing meanwhile, so hand the alert on and don't lock it again.
    pub fn set_on_alert(&self, on_alert: impl Fn(Alert) + Send + Sync + 'static) {
        *self.shared.on_alert.write().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(on_alert));
    }

    /// Which events are alerts, for hosts without alerts of their own. New limits
    /// are judged afresh.
    pub fn set_alerts(&self, alerts: AlertSettings) {
        let mut state = self.shared.write();
        let limits_changed = state.alerts.limits != alerts.limits;
        state.alerts = alerts;
        if limits_changed {
            for host in state.hosts.iter_mut().filter(|h| h.alerts == HostAlerts::Usual) {
                host.watch.reset();
            }
        }
        drop(state);
        self.shared.notify();
    }

    /// Give a host alerts of its own, or the usual ones again.
    pub fn set_host_alerts(&self, id: HostId, alerts: HostAlerts) {
        let mut state = self.shared.write();
        if let Some(host) = state.host_mut(id)
            && host.alerts != alerts
        {
            host.alerts = alerts;
            host.watch.reset();
            host.version += 1;
        }
        drop(state);
        self.shared.notify();
    }

    /// Lock the state for reading. Keep the guard short-lived: probes wait on it.
    /// Never lock it again while holding it (see [`StateGuard`]).
    pub fn read(&self) -> StateGuard<'_> {
        self.shared.read()
    }

    /// Show `name` for a network (by AS number) instead of the one it would get;
    /// `None` goes back to that. Networks with the same name count as one ISP, so a
    /// switch between them isn't reported.
    pub fn set_network_name(&self, asn: u32, name: Option<String>) {
        self.shared.write().networks.set_name(asn, name);
        self.shared.notify();
    }

    /// Find out who runs each public router in `addrs` that isn't known yet, while
    /// network lookups are on, as the route checks do for the routers they find.
    /// The answers land in the state's [`Networks`].
    pub fn look_up_owners(&self, addrs: impl IntoIterator<Item = IpAddr>) {
        let Some(runtime) = self.runtime.as_ref() else { return };
        let wanted: Vec<IpAddr> = {
            let state = self.shared.read();
            if !state.settings.network_lookups {
                return;
            }
            addrs
                .into_iter()
                .filter(|&addr| state.networks.owner_wanted(addr))
                .collect()
        };
        let _runtime = runtime.enter();
        for addr in wanted {
            network::look_up_owner(&self.shared, addr);
        }
    }

    /// For UI tests: change the state directly.
    #[doc(hidden)]
    pub fn edit_for_test(&self, edit: impl FnOnce(&mut State)) {
        edit(&mut self.shared.write());
        self.shared.notify();
    }

    /// For UI tests: a host that is never probed, for the test to fill in.
    #[doc(hidden)]
    pub fn add_for_test(&self, spec: TargetSpec) -> HostId {
        let id = HostId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.shared.write().hosts.push(Host::new(id, spec, now_secs()));
        self.shared.notify();
        id
    }

    pub fn settings(&self) -> ProbeSettings {
        *self.shared.settings_tx.borrow()
    }

    pub fn update_settings(&self, change: impl FnOnce(&mut ProbeSettings)) {
        let mut settings = self.settings();
        change(&mut settings);
        let mut state = self.shared.write();
        state.settings = settings;
        if !settings.network_lookups {
            // Nothing keeps a host's way out up to date now. A check still out when
            // this changed finds lookups off, and keeps what it saw to itself.
            for host in &mut state.hosts {
                host.exit = None;
            }
        }
        drop(state);
        self.shared.settings_tx.send_replace(settings);
        self.shared.notify();
    }

    pub fn add(&self, spec: TargetSpec) -> HostId {
        let id = HostId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.shared.write().hosts.push(Host::new(id, spec.clone(), now_secs()));
        let runtime = self.runtime.as_ref().expect("monitor is running");
        let task = runtime.spawn(run_host(self.shared.clone(), id, spec));
        self.tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, task);
        self.shared.notify();
        id
    }

    pub fn remove(&self, id: HostId) {
        if let Some(task) = self.tasks.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
            task.abort();
        }
        self.shared.write().hosts.retain(|h| h.id != id);
        self.shared.notify();
    }

    /// Show `label` for a host instead of its address, or the address again with
    /// `None`. Spaces are tidied as in the add box. The host keeps its samples, and
    /// events already logged keep the name they have.
    pub fn set_label(&self, id: HostId, label: Option<String>) {
        let label = label
            .map(|label| label.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|label| !label.is_empty());
        if let Some(host) = self.shared.write().host_mut(id) {
            host.spec.label = label;
            host.version += 1;
        }
        self.shared.notify();
    }

    /// Move a host to another position in the list.
    pub fn move_host(&self, id: HostId, to_index: usize) {
        let mut state = self.shared.write();
        if let Some(from) = state.hosts.iter().position(|h| h.id == id) {
            let host = state.hosts.remove(from);
            let to_index = to_index.min(state.hosts.len());
            state.hosts.insert(to_index, host);
        }
        drop(state);
        self.shared.notify();
    }

    /// Trace the route to `target` until the returned [`Trace`] is dropped.
    pub fn trace(&self, target: IpAddr, settings: TraceSettings) -> Trace {
        let runtime = self.runtime.as_ref().expect("monitor is running");
        Trace::start(runtime, self.shared.clone(), target, settings)
    }

    /// For UI tests: a route that is never probed, filled in with [`Trace::record_for_test`].
    #[doc(hidden)]
    pub fn trace_for_test(&self, target: IpAddr) -> Trace {
        let runtime = self.runtime.as_ref().expect("monitor is running");
        Trace::idle(runtime, self.shared.clone(), target)
    }

    /// Forget all samples and events; hosts keep being probed. Each host's current
    /// way out is kept, but not its earlier switches.
    pub fn clear_history(&self) {
        let mut state = self.shared.write();
        for host in &mut state.hosts {
            host.history.clear();
            host.next_to_process = host.history.next_seq();
            host.loss_streak = 0;
            host.watch.restart(host.next_to_process);
            host.exit_changes.clear();
            host.paths.clear();
            host.version += 1;
        }
        state.events.clear();
        drop(state);
        self.shared.notify();
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // Don't wait for probes that are still blocked on their timeout.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

async fn run_host(shared: Arc<Shared>, id: HostId, spec: TargetSpec) {
    let mut settings = shared.settings_tx.subscribe();

    let addr = loop {
        match resolve(&spec).await {
            Ok(addr) => {
                if !shared.resolved(id, addr) {
                    return;
                }
                break addr;
            }
            Err(error) => {
                if !shared.resolve_failed(id, error) {
                    return;
                }
                tokio::time::sleep(RESOLVE_RETRY).await;
            }
        }
    };
    // Follow which network the traffic leaves through, for as long as this host is probed.
    let _exit_watch = AbortOnDrop(tokio::spawn(exit::watch(shared.clone(), id, addr)));

    // This host's probes still out; while one is, the host gets fewer slots.
    let out = Arc::new(AtomicUsize::new(0));
    let mut last_sent: Option<Instant> = None;
    loop {
        let s = *settings.borrow_and_update();
        if s.paused {
            if settings.changed().await.is_err() {
                return;
            }
            last_sent = None;
            continue;
        }
        let due = last_sent.map_or_else(Instant::now, |t| t + s.interval);
        tokio::select! {
            _ = tokio::time::sleep_until(due) => {}
            changed = settings.changed() => {
                if changed.is_err() {
                    return;
                }
                continue;
            }
        }
        // Keep a steady cadence, but after a long stall (laptop sleep) start fresh
        // instead of firing a burst of catch-up probes.
        let now = Instant::now();
        last_sent = Some(if now.duration_since(due) > s.interval { now } else { due });

        let t = crate::now_secs();
        // Too many probes out (lots of hosts that don't answer, at a short
        // interval): skip this one rather than queue it and fall behind.
        let Some(slot) = shared.take_slot(&out) else {
            shared.write().probes_skipped_at = Some(t);
            continue;
        };
        let Some(seq) = shared.begin_probe(id, t) else {
            return;
        };
        let task_shared = shared.clone();
        // A probe already sent runs to its end even if the host is removed:
        // Windows can't take it back, and finish_probe drops what it brings.
        tokio::spawn(async move {
            let probe = tokio::task::spawn_blocking(move || {
                // The slot is given back once Windows returns, answered or not.
                let _slot = slot;
                icmp::echo(addr, s.timeout, s.ttl, s.payload_len)
            });
            // Every probe gets an outcome: the samples after it wait for this one.
            let reply = probe.await.unwrap_or_else(|_| EchoReply::general_failure());
            task_shared.finish_probe(id, seq, reply, s.down_after);
        });
    }
}

/// Aborts a task when dropped, so it lives only as long as whatever holds this.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn resolve(spec: &TargetSpec) -> Result<IpAddr, String> {
    // A range or block left as typed would only fail as a name, less helpfully.
    if let Some(problem) = crate::target::range_problem(&spec.host) {
        return Err(problem);
    }
    let host = spec.host.trim_start_matches('[').trim_end_matches(']').to_string();
    let family = spec.family;
    // An address needs no lookup, but it still has to be of the family asked for.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return pick(family, &[ip]);
    }
    tokio::task::spawn_blocking(move || {
        let addrs = (host.as_str(), 0u16)
            .to_socket_addrs()
            .map_err(|e| without_os_error(&e.to_string()))?;
        let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
        pick(family, &ips)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The address to probe out of a host's addresses: one of `family`, and IPv4 when
/// either will do.
fn pick(family: Family, ips: &[IpAddr]) -> Result<IpAddr, String> {
    let pick = match family {
        Family::V4 => ips.iter().find(|ip| ip.is_ipv4()),
        Family::V6 => ips.iter().find(|ip| ip.is_ipv6()),
        Family::Any => ips.iter().find(|ip| ip.is_ipv4()).or(ips.first()),
    };
    pick.copied().ok_or_else(|| match family {
        Family::V4 => "No IPv4 address".to_string(),
        Family::V6 => "No IPv6 address".to_string(),
        Family::Any => "No address".to_string(),
    })
}

/// "No such host is known. (os error 11001)" -> "No such host is known."
fn without_os_error(message: &str) -> String {
    match message.find(" (os error") {
        Some(i) => message[..i].to_string(),
        None => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::Limits;
    use crate::icmp::EchoStatus;

    fn reply(status: EchoStatus) -> EchoReply {
        EchoReply {
            status,
            from: None,
            elapsed_ms: 5.0,
            windows_ms: 5,
        }
    }

    fn shared_with_host() -> (Shared, HostId) {
        let shared = Shared::new(ProbeSettings::default());
        let id = HostId(1);
        shared
            .write()
            .hosts
            .push(Host::new(id, TargetSpec::new("10.0.0.1"), 0.0));
        (shared, id)
    }

    #[tokio::test]
    async fn an_address_must_match_the_family_asked_for() {
        async fn resolve_text(text: &str) -> Result<IpAddr, String> {
            resolve(&crate::parse_targets(text)[0]).await
        }
        assert_eq!(resolve_text("1.1.1.1").await, Ok("1.1.1.1".parse().unwrap()));
        assert_eq!(resolve_text("ipv4:1.1.1.1").await, Ok("1.1.1.1".parse().unwrap()));
        assert_eq!(resolve_text("ipv6:1.1.1.1").await, Err("No IPv6 address".to_string()));
        assert_eq!(resolve_text("ipv4:::1").await, Err("No IPv4 address".to_string()));
        assert_eq!(resolve_text("ipv6:[::1]").await, Ok("::1".parse().unwrap()));
        assert_eq!(
            resolve_text("10.0.0.0/16").await,
            Err("Too many addresses: the biggest block is a /24".to_string())
        );
    }

    #[test]
    fn estimate_stays_inside_windows_bracket() {
        let at = |windows_ms, elapsed_ms| EchoReply {
            status: EchoStatus::Reply,
            from: None,
            elapsed_ms,
            windows_ms,
        };
        assert_eq!(estimate_rtt_ms(&at(45, 45.8), 0.5), 45.3);
        assert_eq!(estimate_rtt_ms(&at(45, 45.2), 0.5), 45.0);
        assert!((estimate_rtt_ms(&at(1, 3.1), 0.5) - 1.999).abs() < 1e-6);
        assert!((estimate_rtt_ms(&at(0, 0.9), 0.6) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn goes_down_after_streak_and_reports_outage_length() {
        let (shared, id) = shared_with_host();
        for (t, status) in [
            (1.0, EchoStatus::Reply),
            (2.0, EchoStatus::TimedOut),
            (3.0, EchoStatus::TimedOut),
            (4.0, EchoStatus::TimedOut),
            (5.0, EchoStatus::TimedOut),
            (6.0, EchoStatus::Reply),
        ] {
            let seq = shared.begin_probe(id, t).unwrap();
            shared.finish_probe(id, seq, reply(status), 3);
        }
        let state = shared.state.read().unwrap();
        let kinds: Vec<_> = state.events.iter().map(|e| (e.at, e.kind.clone())).collect();
        assert_eq!(
            kinds,
            [(2.0, EventKind::Down), (6.0, EventKind::Up { outage_secs: 4.0 })]
        );
        assert_eq!(state.hosts[0].status, HostStatus::Up);
    }

    #[test]
    fn probes_out_are_capped_and_hosts_that_dont_answer_wait_first() {
        let shared = Arc::new(Shared::new(ProbeSettings::default()));
        let host = || Arc::new(AtomicUsize::new(0));
        // A host that doesn't answer keeps probes out, so it's held back first.
        let silent = host();
        let mut slots: Vec<ProbeSlot> = std::iter::from_fn(|| shared.take_slot(&silent)).collect();
        assert_eq!(slots.len(), MAX_PROBES_OUT_WAITING);
        // Hosts with nothing out still get a slot, up to the overall cap.
        slots.extend(std::iter::from_fn(|| shared.take_slot(&host())));
        assert_eq!(slots.len(), MAX_PROBES_OUT);
        assert!(shared.take_slot(&host()).is_none());
        // Each probe gives its slot back when it ends.
        slots.pop();
        assert!(shared.take_slot(&host()).is_some());
        drop(slots);
        assert_eq!(shared.probes_out.load(Ordering::Relaxed), 0);
        assert_eq!(silent.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn turning_lookups_off_drops_each_hosts_way_out() {
        let monitor = Monitor::new(ProbeSettings::default());
        let id = monitor.add_for_test(TargetSpec::new("10.0.0.1"));
        let exit = Exit {
            asn: 3356,
            router: IpAddr::from([4, 69, 0, 1]),
            hop: 3,
            since: 0.0,
            seen: 0.0,
        };
        monitor.edit_for_test(|state| state.hosts[0].exit = Some(exit));
        let has_exit = || monitor.read().host(id).is_some_and(|host| host.exit.is_some());
        monitor.update_settings(|s| s.interval = Duration::from_secs(2));
        assert!(has_exit(), "other changes keep it");
        monitor.update_settings(|s| s.network_lookups = false);
        assert!(!has_exit());
    }

    /// Sends a probe a second for each status, starting at `t0`, and finishes each one.
    fn probe_all(shared: &Shared, id: HostId, t0: f64, statuses: impl IntoIterator<Item = EchoStatus>) {
        for (i, status) in statuses.into_iter().enumerate() {
            let seq = shared.begin_probe(id, t0 + i as f64).unwrap();
            shared.finish_probe(id, seq, reply(status), 3);
        }
    }

    /// What the alert hook hears, from now on.
    fn listen(shared: &Shared) -> Arc<Mutex<Vec<EventKind>>> {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        *shared.on_alert.write().unwrap() = Some(Box::new(move |alert: Alert| {
            sink.lock().unwrap().push(alert.event.kind);
        }));
        heard
    }

    fn logged(shared: &Shared) -> Vec<(f64, EventKind)> {
        shared.read().events.iter().map(|e| (e.at, e.kind.clone())).collect()
    }

    #[test]
    fn outages_are_alerts_unless_turned_off() {
        let (shared, id) = shared_with_host();
        let heard = listen(&shared);
        let outage = [
            EchoStatus::Reply,
            EchoStatus::TimedOut,
            EchoStatus::TimedOut,
            EchoStatus::TimedOut,
            EchoStatus::Reply,
        ];
        probe_all(&shared, id, 0.0, outage);
        assert_eq!(
            *heard.lock().unwrap(),
            [EventKind::Down, EventKind::Up { outage_secs: 3.0 }]
        );

        heard.lock().unwrap().clear();
        shared.write().hosts[0].alerts = HostAlerts::Off;
        probe_all(&shared, id, 10.0, outage);
        shared.write().hosts[0].alerts = HostAlerts::Usual;
        shared.write().alerts.outages = false;
        probe_all(&shared, id, 20.0, outage);
        assert!(heard.lock().unwrap().is_empty(), "{:?}", heard.lock().unwrap());
        assert_eq!(logged(&shared).len(), 6, "every outage is logged all the same");
    }

    #[test]
    fn a_host_that_never_answered_raises_no_outage_alert() {
        let (shared, id) = shared_with_host();
        let heard = listen(&shared);
        // An unused address, say: it goes down without having stopped answering, and
        // its first reply ends no outage anyone was told about.
        probe_all(&shared, id, 0.0, [EchoStatus::TimedOut; 4]);
        probe_all(&shared, id, 4.0, [EchoStatus::Reply]);
        assert!(heard.lock().unwrap().is_empty(), "{:?}", heard.lock().unwrap());
        assert_eq!(logged(&shared).len(), 2, "both are logged all the same");

        // Once it has answered, an outage is an alert.
        let outage = [
            EchoStatus::TimedOut,
            EchoStatus::TimedOut,
            EchoStatus::TimedOut,
            EchoStatus::Reply,
        ];
        probe_all(&shared, id, 5.0, outage);
        assert_eq!(
            *heard.lock().unwrap(),
            [EventKind::Down, EventKind::Up { outage_secs: 3.0 }]
        );
    }

    #[test]
    fn alerts_name_a_renamed_host_by_its_new_name() {
        let monitor = Monitor::new(ProbeSettings::default());
        let id = monitor.add_for_test(TargetSpec::new("10.0.0.1"));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        monitor.set_on_alert(move |alert| {
            let Event {
                host_name,
                host_address,
                ..
            } = alert.event;
            sink.lock().unwrap().push((host_name, host_address));
        });
        probe_all(&monitor.shared, id, 0.0, [EchoStatus::Reply]);
        monitor.set_label(id, Some("Office edge".into()));
        probe_all(&monitor.shared, id, 1.0, [EchoStatus::TimedOut; 3]);
        assert_eq!(
            *heard.lock().unwrap(),
            [("Office edge".to_string(), "10.0.0.1".to_string())]
        );
    }

    #[test]
    fn scattered_loss_is_an_alert_until_it_has_stayed_away() {
        let (shared, id) = shared_with_host();
        let heard = listen(&shared);
        // The usual limit would let this host be; its own doesn't.
        shared.write().alerts.limits = Limits {
            pings: 10,
            loss_pct: Some(50.0),
            slow_ms: None,
        };
        shared.write().hosts[0].alerts = HostAlerts::Own(Limits {
            pings: 10,
            loss_pct: Some(20.0),
            slow_ms: None,
        });
        let mut statuses = vec![EchoStatus::Reply; 10];
        statuses.extend([
            EchoStatus::TimedOut,
            EchoStatus::Reply,
            EchoStatus::TimedOut,
            EchoStatus::Reply,
        ]);
        statuses.extend([EchoStatus::Reply; 30]);
        probe_all(&shared, id, 0.0, statuses);
        // The alert comes at the reply after the second loss. The first loss leaves
        // the window at ping 20, and the alert ends after ten replies within the limit.
        let loss = EventKind::Loss {
            lost: 2,
            pings: 10,
            limit_pct: 20.0,
        };
        let ended = EventKind::LossEnded {
            limit_pct: 20.0,
            pings: 10,
        };
        assert_eq!(logged(&shared), [(13.0, loss.clone()), (29.0, ended.clone())]);
        assert_eq!(*heard.lock().unwrap(), [loss, ended]);
    }

    #[test]
    fn an_outage_is_its_own_alert_and_its_loss_is_forgotten() {
        let (shared, id) = shared_with_host();
        shared.write().alerts.limits = Limits {
            pings: 5,
            loss_pct: Some(40.0),
            slow_ms: None,
        };
        // Two lost pings would be over the limit, but the host was going down.
        let mut statuses = vec![EchoStatus::Reply; 5];
        statuses.extend([EchoStatus::TimedOut; 6]);
        statuses.extend([EchoStatus::Reply; 10]);
        probe_all(&shared, id, 0.0, statuses);
        assert_eq!(
            logged(&shared),
            [(5.0, EventKind::Down), (11.0, EventKind::Up { outage_secs: 6.0 })]
        );
    }

    #[test]
    fn a_renamed_host_keeps_its_samples_and_its_events_their_names() {
        let monitor = Monitor::new(ProbeSettings::default());
        let id = monitor.add_for_test(TargetSpec::new("10.0.0.1"));
        let probe = |t: f64, status| {
            let seq = monitor.shared.begin_probe(id, t).unwrap();
            monitor.shared.finish_probe(id, seq, reply(status), 1);
        };
        probe(1.0, EchoStatus::TimedOut);
        monitor.set_label(id, Some("  Office   edge ".into()));
        probe(2.0, EchoStatus::Reply);
        {
            let state = monitor.read();
            let host = state.host(id).unwrap();
            assert_eq!(host.name(), "Office edge", "spaces are tidied");
            assert_eq!(host.history.len(), 2);
            let logged: Vec<_> = state
                .events
                .iter()
                .map(|e| (e.host_name.as_str(), e.host_address.as_str()))
                .collect();
            assert_eq!(logged, [("10.0.0.1", "10.0.0.1"), ("Office edge", "10.0.0.1")]);
        }
        monitor.set_label(id, Some(" ".into()));
        assert_eq!(
            monitor.read().host(id).unwrap().spec.label,
            None,
            "a blank name is none"
        );
    }

    #[test]
    fn short_loss_is_not_an_outage() {
        let (shared, id) = shared_with_host();
        for (t, status) in [
            (1.0, EchoStatus::Reply),
            (2.0, EchoStatus::TimedOut),
            (3.0, EchoStatus::TimedOut),
            (4.0, EchoStatus::Reply),
        ] {
            let seq = shared.begin_probe(id, t).unwrap();
            shared.finish_probe(id, seq, reply(status), 3);
        }
        assert!(shared.state.read().unwrap().events.is_empty());
    }

    #[test]
    fn events_stay_in_time_order() {
        let (shared, id) = shared_with_host();
        let mut state = shared.write();
        for at in [1.0, 5.0, 3.0, 5.0] {
            state.push_event(Event {
                at,
                host: id,
                host_name: "host".into(),
                host_address: "10.0.0.1".into(),
                kind: EventKind::Down,
            });
        }
        let times: Vec<f64> = state.events.iter().map(|e| e.at).collect();
        assert_eq!(times, [1.0, 3.0, 5.0, 5.0]);
    }

    #[test]
    fn out_of_order_results_are_processed_in_send_order() {
        let (shared, id) = shared_with_host();
        let seqs: Vec<u64> = (1..=4).map(|t| shared.begin_probe(id, t as f64).unwrap()).collect();
        // The three timeouts finish after the later reply arrives.
        shared.finish_probe(id, seqs[3], reply(EchoStatus::Reply), 3);
        assert_eq!(shared.state.read().unwrap().hosts[0].status, HostStatus::Resolving);
        for &seq in &seqs[..3] {
            shared.finish_probe(id, seq, reply(EchoStatus::TimedOut), 3);
        }
        let state = shared.state.read().unwrap();
        let kinds: Vec<_> = state.events.iter().map(|e| e.kind.clone()).collect();
        assert_eq!(kinds, [EventKind::Down, EventKind::Up { outage_secs: 3.0 }]);
    }
}
