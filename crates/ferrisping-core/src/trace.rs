//! Continuous traceroute: every hop on the way to a host, probed again and again
//! (like mtr or pathping) so loss and delay can be pinned on one hop.
//!
//! Each probe is an ICMP echo sent with a small TTL. The router where the TTL runs
//! out answers "TTL expired", which tells us who that hop is; once the TTL is big
//! enough to get there, the target answers the echo itself. Windows' `tracert`
//! works the same way, so this needs no administrator rights either.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use dns_lookup::LookupErrorKind;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::engine::{Shared, estimate_rtt_ms};
use crate::icmp::{self, EchoReply, EchoStatus};
use crate::network;
use crate::now_secs;

/// How many recent results each hop keeps for its history strip.
pub const RECENT_RESULTS: usize = 60;
/// Once every hop has been tried without reaching the target, keep probing this
/// many hops past the last one that answered, in case they start answering.
const TRAILING_SILENT: u8 = 3;
/// A hop that leaves this many probes in a row unanswered has gone quiet. One that
/// said the target can't be reached is then just quiet: the route may be back,
/// through a router that doesn't answer traceroutes. A target that goes quiet may
/// be down, or further away now, behind such a router.
const QUIET: u8 = 5;
/// While the target isn't answering, every this many rounds probe all the way out,
/// in case it's back behind routers that don't answer traceroutes.
const SWEEP_EVERY: u32 = 10;
/// How long before asking again for a hop's name when DNS didn't answer.
const NAME_RETRY: Duration = Duration::from_secs(60);

/// How a route is probed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TraceSettings {
    /// Time for one round, which probes every hop once.
    pub interval: Duration,
    pub timeout: Duration,
    /// Stop looking for the target this many hops out. `tracert` stops at 30, but a
    /// few paths are longer (bad.horse, a traceroute joke, is nearly 40 hops away).
    pub max_hops: u8,
    pub payload_len: u16,
}

impl Default for TraceSettings {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(2),
            max_hops: 64,
            payload_len: 32,
        }
    }
}

/// An address that answered for a hop. Load-balanced paths have several per hop.
#[derive(Clone, Debug, PartialEq)]
pub struct HopAddr {
    pub addr: IpAddr,
    pub replies: u32,
}

/// Everything measured at one distance from us.
#[derive(Clone, Debug, Default)]
pub struct Hop {
    /// Who answered at this distance, most answers first.
    pub addrs: Vec<HopAddr>,
    /// Finished probes; ones still waiting for an answer aren't counted.
    pub sent: u32,
    pub received: u32,
    pub last_ms: Option<f32>,
    pub best_ms: Option<f32>,
    pub worst_ms: Option<f32>,
    /// Recent results, oldest first: the round trip, or `None` for no answer.
    pub recent: VecDeque<Option<f32>>,
    /// A router here reported that the target can't be reached.
    pub unreachable: bool,
    /// Probes in a row with no answer at all.
    missed_in_row: u8,
    sum_ms: f64,
    jitter_sum_ms: f64,
    jitter_count: u32,
}

impl Hop {
    /// The address that answered most often.
    pub fn addr(&self) -> Option<IpAddr> {
        self.addrs.first().map(|a| a.addr)
    }

    pub fn answered(&self) -> bool {
        self.received > 0
    }

    pub fn avg_ms(&self) -> Option<f32> {
        (self.received > 0).then(|| (self.sum_ms / self.received as f64) as f32)
    }

    /// Mean change between consecutive answers, as for hosts.
    pub fn jitter_ms(&self) -> Option<f32> {
        (self.jitter_count > 0).then(|| (self.jitter_sum_ms / self.jitter_count as f64) as f32)
    }

    pub fn loss_pct(&self) -> Option<f32> {
        (self.sent > 0).then(|| (self.sent - self.received) as f32 * 100.0 / self.sent as f32)
    }

    /// Count one finished probe: who answered and how long it took, or nothing.
    fn record(&mut self, answer: Option<(IpAddr, f32)>) {
        self.sent += 1;
        if let Some((addr, rtt_ms)) = answer {
            self.received += 1;
            if let Some(previous) = self.last_ms {
                self.jitter_sum_ms += (rtt_ms - previous).abs() as f64;
                self.jitter_count += 1;
            }
            self.last_ms = Some(rtt_ms);
            self.best_ms = Some(self.best_ms.map_or(rtt_ms, |b| b.min(rtt_ms)));
            self.worst_ms = Some(self.worst_ms.map_or(rtt_ms, |w| w.max(rtt_ms)));
            self.sum_ms += rtt_ms as f64;
            match self.addrs.iter_mut().find(|a| a.addr == addr) {
                Some(known) => known.replies += 1,
                None => self.addrs.push(HopAddr { addr, replies: 1 }),
            }
            self.addrs.sort_by_key(|a| std::cmp::Reverse(a.replies));
        }
        if self.recent.len() == RECENT_RESULTS {
            self.recent.pop_front();
        }
        self.recent.push_back(answer.map(|(_, rtt_ms)| rtt_ms));
    }
}

/// The path to one target, as measured so far.
#[derive(Clone, Debug)]
pub struct Route {
    pub target: IpAddr,
    /// Hop 1, the nearest router, first.
    pub hops: Vec<Hop>,
    /// How many hops away the target answered, once it has.
    pub reached_at: Option<u8>,
    /// Hop names found by reverse DNS.
    pub names: HashMap<IpAddr, String>,
    /// When tracing started, in seconds since the UNIX epoch.
    pub started: f64,
    /// Bumped on every change, so the UI can cache what it derives.
    pub version: u64,
    max_hops: u8,
    /// Addresses whose names have been asked for, so each is asked once. One whose
    /// lookup failed is dropped again later, to be asked the next time it answers.
    looked_up: HashSet<IpAddr>,
}

impl Route {
    fn new(target: IpAddr, max_hops: u8) -> Self {
        Self {
            target,
            hops: Vec::new(),
            reached_at: None,
            names: HashMap::new(),
            started: now_secs(),
            version: 0,
            max_hops: max_hops.max(1),
            looked_up: HashSet::new(),
        }
    }

    /// The hop where a router reported the target can't be reached, if any.
    pub fn unreachable_at(&self) -> Option<u8> {
        self.hops.iter().position(|h| h.unreachable).map(|i| i as u8 + 1)
    }

    /// Whether some distances haven't been tried yet, so the target may still turn up.
    pub fn searching(&self) -> bool {
        self.reached_at.is_none() && self.unreachable_at().is_none() && !self.tried_all()
    }

    fn tried_all(&self) -> bool {
        self.hops.len() >= self.max_hops as usize && self.hops.iter().all(|h| h.sent > 0)
    }

    /// Whether the target stopped answering where it did: it may be down, or
    /// further away now, behind a router that doesn't answer traceroutes.
    fn target_quiet(&self) -> bool {
        self.reached_at
            .and_then(|reached| self.hops.get(reached as usize - 1))
            .is_some_and(|hop| hop.missed_in_row >= QUIET)
    }

    /// How many hops a round probes. While the target isn't answering, a round
    /// that `sweep`s goes all the way out, past routers that don't answer.
    pub fn probe_limit(&self, sweep: bool) -> u8 {
        // How far out something answers, while the target doesn't.
        let farthest = match self.reached_at {
            // Probes past a target that answers just reach it again.
            Some(reached) if !self.target_quiet() => return reached,
            // One that went quiet may have moved further away.
            Some(reached) => reached,
            None => {
                if let Some(end) = self.unreachable_at() {
                    return end;
                }
                // Until every distance has been tried once, the target could be anywhere.
                if !self.tried_all() {
                    return self.max_hops;
                }
                self.last_answered()
            }
        };
        if sweep {
            self.max_hops
        } else {
            farthest.saturating_add(TRAILING_SILENT).min(self.max_hops)
        }
    }

    /// The farthest hop that has answered, or 0.
    pub fn last_answered(&self) -> u8 {
        self.hops.iter().rposition(Hop::answered).map_or(0, |i| i as u8 + 1)
    }

    /// The hops worth showing: up to the target, or one past the last hop that
    /// answered when the target doesn't (yet).
    pub fn shown_hops(&self) -> &[Hop] {
        let end = match self.reached_at.or(self.unreachable_at()) {
            Some(end) => end as usize,
            None => self.last_answered() as usize + 1,
        };
        &self.hops[..end.min(self.hops.len())]
    }

    /// The name to show for an address: its reverse DNS name, or the address.
    pub fn name(&self, addr: IpAddr) -> String {
        self.names.get(&addr).cloned().unwrap_or_else(|| addr.to_string())
    }

    /// Count one finished probe that was sent with `ttl`. Returns an address
    /// that answered for the first time, so its name can be looked up.
    fn record(&mut self, ttl: u8, reply: &EchoReply, rtt_ms: f32) -> Option<IpAddr> {
        // Probes go past the target before it first answers that close, and while
        // it's quiet, to see whether it moved further away.
        let past_target = self.reached_at.is_some_and(|reached| ttl > reached);
        let answer = match reply.status {
            // The target answering from past where it went quiet: the path got
            // longer. The next round finds where it ends now.
            EchoStatus::Reply if past_target && self.target_quiet() => {
                self.target_moved_away();
                self.version += 1;
                return None;
            }
            // Otherwise probes past the target reach the target too and add nothing.
            EchoStatus::Reply if past_target => return None,
            EchoStatus::Reply => {
                // Closer than before (or for the first time): the path ends here, and
                // whatever routers said before, nothing on the way blocks it now.
                self.reached_at = Some(ttl);
                self.hops.truncate(ttl as usize);
                for hop in &mut self.hops {
                    hop.unreachable = false;
                }
                Some(reply.from.unwrap_or(self.target))
            }
            EchoStatus::TtlExpired => {
                // A router where the target answered, or past it: the path got longer.
                if self.reached_at.is_some_and(|reached| ttl >= reached) {
                    self.target_moved_away();
                }
                reply.from
            }
            // Anything else from past the target says nothing about the way there.
            _ if past_target => return None,
            EchoStatus::Unreachable => reply.from,
            EchoStatus::TimedOut | EchoStatus::Failed(_) => None,
        };
        if self.hops.len() < ttl as usize {
            self.hops.resize_with(ttl as usize, Hop::default);
        }
        let hop = &mut self.hops[ttl as usize - 1];
        match reply.status {
            EchoStatus::Unreachable => {
                hop.unreachable = true;
                hop.missed_in_row = 0;
            }
            EchoStatus::Reply | EchoStatus::TtlExpired => {
                hop.unreachable = false;
                hop.missed_in_row = 0;
            }
            EchoStatus::TimedOut | EchoStatus::Failed(_) => {
                hop.missed_in_row = hop.missed_in_row.saturating_add(1);
                if hop.missed_in_row >= QUIET {
                    hop.unreachable = false;
                }
            }
        }
        hop.record(answer.map(|addr| (addr, rtt_ms)));
        self.version += 1;
        answer.filter(|addr| self.looked_up.insert(*addr))
    }

    /// The target is further away than where it answered: look for it again. If
    /// it had gone quiet there, what that hop measured was the target, so the hop
    /// starts over.
    fn target_moved_away(&mut self) {
        let quiet = self.target_quiet();
        if let Some(reached) = self.reached_at.take()
            && quiet
        {
            self.hops[reached as usize - 1] = Hop::default();
        }
    }
}

struct TraceShared {
    route: RwLock<Route>,
    engine: Arc<Shared>,
}

impl TraceShared {
    fn read(&self) -> RwLockReadGuard<'_, Route> {
        self.route.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Route> {
        self.route.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn finish(self: &Arc<Self>, ttl: u8, reply: EchoReply) {
        let rtt_ms = estimate_rtt_ms(&reply, self.engine.overhead_ms());
        let new_addr = self.write().record(ttl, &reply, rtt_ms);
        if let Some(addr) = new_addr {
            self.look_up_name(addr);
            network::look_up_owner(&self.engine, addr);
        }
        self.engine.notify();
    }

    /// Ask again who owns the hops whose owners couldn't be found (while offline, say).
    fn retry_failed_owners(&self) {
        // Never hold the route while locking the engine's state: the UI locks them
        // the other way round.
        let addrs: Vec<IpAddr> = self
            .read()
            .hops
            .iter()
            .flat_map(|hop| hop.addrs.iter().map(|a| a.addr))
            .collect();
        let failed: Vec<IpAddr> = {
            let state = self.engine.read();
            addrs.into_iter().filter(|&a| state.networks.owner_failed(a)).collect()
        };
        for addr in failed {
            network::look_up_owner(&self.engine, addr);
        }
    }

    fn look_up_name(self: &Arc<Self>, addr: IpAddr) {
        let shared = Arc::clone(self);
        tokio::spawn(async move {
            match tokio::task::spawn_blocking(move || reverse_dns(addr)).await {
                Ok(Ok(Some(name))) => {
                    let mut route = shared.write();
                    route.names.insert(addr, name);
                    route.version += 1;
                    drop(route);
                    shared.engine.notify();
                }
                Ok(Ok(None)) => {}
                // DNS didn't answer (while offline, say): ask again the next time
                // this address answers, after a pause.
                _ => {
                    tokio::time::sleep(NAME_RETRY).await;
                    shared.write().looked_up.remove(&addr);
                }
            }
        });
    }
}

/// An address's name from reverse DNS: `Ok(None)` when it has none, and `Err`
/// when DNS couldn't say.
fn reverse_dns(addr: IpAddr) -> Result<Option<String>, ()> {
    match dns_lookup::lookup_addr(&addr) {
        // Some resolvers hand back the address itself instead of saying it has no name.
        Ok(name) => Ok(name.parse::<IpAddr>().is_err().then_some(name)),
        Err(error) if matches!(error.kind(), LookupErrorKind::NoName | LookupErrorKind::NoData) => Ok(None),
        Err(_) => Err(()),
    }
}

/// A route being traced. Probing stops when this is dropped.
pub struct Trace {
    shared: Arc<TraceShared>,
    task: JoinHandle<()>,
}

impl Trace {
    pub(crate) fn start(runtime: &Runtime, engine: Arc<Shared>, target: IpAddr, settings: TraceSettings) -> Self {
        let shared = Arc::new(TraceShared {
            route: RwLock::new(Route::new(target, settings.max_hops)),
            engine,
        });
        let task = runtime.spawn(run(Arc::clone(&shared), settings));
        Self { shared, task }
    }

    /// A route that is never probed, for tests to fill in.
    pub(crate) fn idle(runtime: &Runtime, engine: Arc<Shared>, target: IpAddr) -> Self {
        let shared = Arc::new(TraceShared {
            route: RwLock::new(Route::new(target, TraceSettings::default().max_hops)),
            engine,
        });
        let task = runtime.spawn(async {});
        Self { shared, task }
    }

    pub fn target(&self) -> IpAddr {
        self.read().target
    }

    /// Lock the route for reading. Keep the guard short-lived: probes wait on it.
    pub fn read(&self) -> RwLockReadGuard<'_, Route> {
        self.shared.read()
    }

    /// Forget what was measured and start over. Hop names are kept.
    pub fn restart(&self) {
        let mut route = self.shared.write();
        let fresh = Route {
            names: std::mem::take(&mut route.names),
            looked_up: std::mem::take(&mut route.looked_up),
            version: route.version + 1,
            ..Route::new(route.target, route.max_hops)
        };
        // Probes still in flight land in the fresh route; that's one round at most.
        *route = fresh;
        drop(route);
        self.shared.engine.notify();
    }

    /// For UI tests: count one probe sent with `ttl` as if it came back, answered by
    /// an address after so many milliseconds, or not at all. Nothing is looked up.
    #[doc(hidden)]
    pub fn record_for_test(&self, ttl: u8, answer: Option<(IpAddr, f32)>) {
        let mut route = self.shared.write();
        let status = match answer {
            Some((from, _)) if from == route.target => EchoStatus::Reply,
            Some(_) => EchoStatus::TtlExpired,
            None => EchoStatus::TimedOut,
        };
        let rtt_ms = answer.map_or(0.0, |(_, rtt_ms)| rtt_ms);
        let reply = EchoReply {
            status,
            from: answer.map(|(from, _)| from),
            elapsed_ms: rtt_ms as f64,
            windows_ms: 0,
        };
        route.record(ttl, &reply, rtt_ms);
        drop(route);
        self.shared.engine.notify();
    }

    /// For UI tests: name an address, as reverse DNS would.
    #[doc(hidden)]
    pub fn name_for_test(&self, addr: IpAddr, name: &str) {
        let mut route = self.shared.write();
        route.names.insert(addr, name.to_string());
        route.version += 1;
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(shared: Arc<TraceShared>, settings: TraceSettings) {
    let mut probe_settings = shared.engine.settings_tx.subscribe();
    let target = shared.read().target;
    let mut rounds: u32 = 0;
    loop {
        // Pausing the monitor pauses traces too.
        if probe_settings.borrow_and_update().paused {
            if probe_settings.changed().await.is_err() {
                return;
            }
            continue;
        }
        let round_start = Instant::now();
        shared.retry_failed_owners();
        rounds = rounds.wrapping_add(1);
        let sweep = rounds.is_multiple_of(SWEEP_EVERY);
        let limit = shared.read().probe_limit(sweep);
        // Spread each round's probes out rather than sending them in a burst.
        let spacing = settings.interval / u32::from(limit);
        for ttl in 1..=limit {
            if ttl > shared.read().probe_limit(sweep) {
                break; // the target turned out to be closer
            }
            let probe = Arc::clone(&shared);
            tokio::spawn(async move {
                let sent = tokio::task::spawn_blocking(move || {
                    icmp::echo(target, settings.timeout, ttl, settings.payload_len)
                });
                probe.finish(ttl, sent.await.unwrap_or_else(|_| EchoReply::general_failure()));
            });
            tokio::time::sleep(spacing).await;
        }
        tokio::time::sleep_until(round_start + settings.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: [u8; 4] = [10, 9, 9, 9];

    fn router(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    fn reply(status: EchoStatus, from: Option<IpAddr>) -> EchoReply {
        EchoReply {
            status,
            from,
            elapsed_ms: 0.0,
            windows_ms: 0,
        }
    }

    fn expired(n: u8) -> EchoReply {
        reply(EchoStatus::TtlExpired, Some(router(n)))
    }

    fn from_target() -> EchoReply {
        reply(EchoStatus::Reply, Some(IpAddr::from(TARGET)))
    }

    fn timed_out() -> EchoReply {
        reply(EchoStatus::TimedOut, None)
    }

    fn route(max_hops: u8) -> Route {
        Route::new(IpAddr::from(TARGET), max_hops)
    }

    #[test]
    fn finds_the_path_and_stops_at_the_target() {
        let mut r = route(30);
        assert_eq!(r.probe_limit(false), 30);
        for ttl in 1..=3 {
            r.record(ttl, &expired(ttl), 5.0);
        }
        r.record(4, &from_target(), 9.0);
        // Probes sent past the target come back from the target as well.
        for ttl in 5..=8 {
            assert_eq!(r.record(ttl, &from_target(), 9.0), None);
        }
        assert_eq!(r.reached_at, Some(4));
        assert_eq!(r.probe_limit(false), 4);
        assert_eq!(r.hops.len(), 4);
        assert_eq!(r.shown_hops().len(), 4);
        assert_eq!(r.hops[3].addr(), Some(IpAddr::from(TARGET)));
    }

    #[test]
    fn notices_when_the_path_gets_longer_or_shorter() {
        let mut r = route(30);
        for ttl in 1..=3 {
            r.record(ttl, &expired(ttl), 5.0);
        }
        r.record(4, &from_target(), 9.0);

        // Hop 4 is now a router: the target moved further away.
        r.record(4, &expired(4), 6.0);
        assert_eq!(r.reached_at, None);
        assert_eq!(r.probe_limit(false), 30);

        r.record(5, &from_target(), 9.0);
        assert_eq!(r.reached_at, Some(5));

        // And now it answers at 3: anything past that is dropped.
        r.record(3, &from_target(), 8.0);
        assert_eq!(r.reached_at, Some(3));
        assert_eq!(r.hops.len(), 3);
    }

    #[test]
    fn unreachable_ends_the_path_until_the_target_answers() {
        let mut r = route(30);
        r.record(1, &expired(1), 1.0);
        r.record(2, &reply(EchoStatus::Unreachable, Some(router(1))), 1.0);
        assert_eq!(r.probe_limit(false), 2);
        assert_eq!(r.shown_hops().len(), 2);
        assert!(r.hops[1].unreachable);

        // A timeout doesn't change that; an answer from the target does.
        r.record(2, &timed_out(), 0.0);
        assert!(r.hops[1].unreachable);
        r.record(2, &from_target(), 2.0);
        assert!(!r.hops[1].unreachable);
        assert_eq!(r.reached_at, Some(2));
    }

    #[test]
    fn the_target_answering_clears_what_routers_said_before() {
        // The route flapped: hop 2 couldn't reach the target, then the target answered.
        let mut r = route(30);
        r.record(1, &expired(1), 1.0);
        r.record(2, &reply(EchoStatus::Unreachable, Some(router(2))), 1.0);
        r.record(4, &from_target(), 4.0);
        assert_eq!(r.reached_at, Some(4));
        assert_eq!(r.unreachable_at(), None);

        // Probes sent past the target before it answered are out of date.
        r.record(6, &reply(EchoStatus::Unreachable, Some(router(5))), 1.0);
        assert_eq!(r.unreachable_at(), None);
        assert_eq!(r.hops.len(), 4);

        // A router that can't reach the target any more is news, though.
        r.record(3, &reply(EchoStatus::Unreachable, Some(router(3))), 1.0);
        assert_eq!(r.unreachable_at(), Some(3));
        // Probing still goes as far as the target, to see it come back.
        assert_eq!(r.probe_limit(false), 4);
    }

    #[test]
    fn a_hop_that_goes_quiet_after_unreachable_is_probed_past_again() {
        let mut r = route(30);
        r.record(1, &expired(1), 1.0);
        r.record(2, &reply(EchoStatus::Unreachable, Some(router(1))), 1.0);
        for _ in 1..QUIET {
            r.record(2, &timed_out(), 0.0);
        }
        assert_eq!(r.probe_limit(false), 2);
        // The route is back, through a router that doesn't answer traceroutes.
        r.record(2, &timed_out(), 0.0);
        assert_eq!(r.unreachable_at(), None);
        assert_eq!(r.probe_limit(false), 30);
    }

    #[test]
    fn a_silent_target_keeps_a_few_hops_past_the_last_answer() {
        let mut r = route(8);
        r.record(1, &expired(1), 1.0);
        r.record(2, &expired(2), 2.0);
        // Before every distance has been tried, keep looking all the way out.
        assert_eq!(r.probe_limit(false), 8);
        assert!(r.searching());
        for ttl in 3..=8 {
            r.record(ttl, &timed_out(), 0.0);
        }
        assert!(!r.searching());
        assert_eq!(r.last_answered(), 2);
        assert_eq!(r.probe_limit(false), 2 + TRAILING_SILENT);
        // Every so often a round still goes all the way out.
        assert_eq!(r.probe_limit(true), 8);
        // One silent hop is shown after the last that answered.
        assert_eq!(r.shown_hops().len(), 3);
    }

    #[test]
    fn a_trace_started_offline_still_finds_the_target() {
        // Nothing answered at first (offline, say).
        let mut r = route(30);
        for ttl in 1..=30 {
            r.record(ttl, &timed_out(), 0.0);
        }
        assert_eq!(r.probe_limit(false), TRAILING_SILENT);
        // Back online, the routers on the way don't answer traceroutes and the
        // target is 8 hops out: a round that sweeps reaches it.
        assert_eq!(r.probe_limit(true), 30);
        r.record(8, &from_target(), 9.0);
        assert_eq!(r.reached_at, Some(8));
        // Sweeps stop at a target that answers.
        assert_eq!(r.probe_limit(true), 8);
    }

    #[test]
    fn a_target_that_goes_quiet_is_looked_for_further_out() {
        let mut r = route(30);
        for ttl in 1..=3 {
            r.record(ttl, &expired(ttl), 5.0);
        }
        r.record(4, &from_target(), 9.0);
        // The path got longer, and the router now 4 hops out doesn't answer.
        for _ in 1..QUIET {
            r.record(4, &timed_out(), 0.0);
        }
        assert_eq!(r.probe_limit(false), 4);
        r.record(4, &timed_out(), 0.0);
        assert_eq!(r.probe_limit(false), 4 + TRAILING_SILENT);
        assert_eq!(r.probe_limit(true), 30);
        // Silence past the target says nothing about it.
        r.record(5, &timed_out(), 0.0);
        assert_eq!(r.hops.len(), 4);

        // The target answers from further out: the hop where it answered before
        // starts over, and the next round looks for it from the first hop.
        r.record(6, &from_target(), 9.0);
        assert_eq!(r.reached_at, None);
        assert_eq!(r.hops[3].sent, 0);
        assert_eq!(r.probe_limit(false), 30);
        r.record(6, &from_target(), 9.0);
        assert_eq!(r.reached_at, Some(6));
    }

    #[test]
    fn a_router_past_a_quiet_target_means_the_path_got_longer() {
        let mut r = route(30);
        r.record(1, &expired(1), 1.0);
        r.record(2, &from_target(), 2.0);
        for _ in 0..QUIET {
            r.record(2, &timed_out(), 0.0);
        }
        r.record(3, &expired(3), 3.0);
        assert_eq!(r.reached_at, None);
        // What hop 2 measured was the target, which isn't there any more.
        assert_eq!(r.hops[1].sent, 0);
        assert_eq!(r.hops[2].addr(), Some(router(3)));
    }

    #[test]
    fn a_target_back_where_it_was_stays_there() {
        // Down for a while (a reboot, say), then answering at the same distance.
        let mut r = route(30);
        r.record(1, &expired(1), 1.0);
        r.record(2, &from_target(), 2.0);
        for _ in 0..QUIET {
            r.record(2, &timed_out(), 0.0);
        }
        assert_eq!(r.probe_limit(false), 2 + TRAILING_SILENT);
        r.record(2, &from_target(), 2.0);
        assert_eq!(r.reached_at, Some(2));
        assert_eq!(r.probe_limit(false), 2);
        // Probes sent past it while it was quiet add nothing now.
        assert_eq!(r.record(3, &from_target(), 2.0), None);
        assert_eq!(r.reached_at, Some(2));
        assert_eq!(r.hops.len(), 2);
    }

    #[test]
    fn silent_hops_in_the_middle_are_shown() {
        let mut r = route(30);
        r.record(1, &expired(1), 1.0);
        r.record(2, &timed_out(), 0.0);
        r.record(3, &expired(3), 3.0);
        r.record(4, &timed_out(), 0.0);
        r.record(5, &timed_out(), 0.0);
        // Hop 2 stays; of the silent hops after hop 3, only the first is shown.
        assert_eq!(r.shown_hops().len(), 4);
        assert!(!r.shown_hops()[1].answered());
    }

    #[test]
    fn hop_statistics() {
        let mut r = route(30);
        for (rtt, answered) in [(10.0, true), (0.0, false), (14.0, true), (12.0, true)] {
            let probe = if answered { expired(1) } else { timed_out() };
            r.record(1, &probe, rtt);
        }
        let hop = &r.hops[0];
        assert_eq!((hop.sent, hop.received), (4, 3));
        assert_eq!(hop.loss_pct(), Some(25.0));
        assert_eq!(
            (hop.best_ms, hop.worst_ms, hop.last_ms),
            (Some(10.0), Some(14.0), Some(12.0))
        );
        assert_eq!(hop.avg_ms(), Some(12.0));
        // |14 - 10| and |12 - 14|: the lost probe in between doesn't count.
        assert_eq!(hop.jitter_ms(), Some(3.0));
        assert_eq!(hop.recent, [Some(10.0), None, Some(14.0), Some(12.0)]);
    }

    #[test]
    fn load_balanced_hops_list_every_address_and_ask_each_name_once() {
        let mut r = route(30);
        assert_eq!(r.record(2, &expired(7), 5.0), Some(router(7)));
        assert_eq!(r.record(2, &expired(8), 5.0), Some(router(8)));
        assert_eq!(r.record(2, &expired(8), 5.0), None);
        let addrs: Vec<_> = r.hops[1].addrs.iter().map(|a| (a.addr, a.replies)).collect();
        assert_eq!(addrs, [(router(8), 2), (router(7), 1)]);
        // Hop 1 hasn't finished a probe yet, but it's listed in its place.
        assert_eq!(r.hops[0].sent, 0);
    }

    #[test]
    fn recent_results_are_capped() {
        let mut r = route(30);
        for _ in 0..RECENT_RESULTS + 5 {
            r.record(1, &expired(1), 1.0);
        }
        assert_eq!(r.hops[0].recent.len(), RECENT_RESULTS);
    }
}
