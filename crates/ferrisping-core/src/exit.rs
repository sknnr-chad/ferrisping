//! Which network each host's traffic leaves through, and when that changes.
//!
//! Every half minute each host gets a light, partial traceroute: one probe per hop,
//! a dozen hops out at most. The first router on a public network marks the way
//! out: the ISP of a home or office line, or whichever of several circuits BGP
//! picked. A different network there two rounds in a row counts as a switch.
//!
//! A host that's down gets a round every couple of minutes instead, in a queue of
//! its own, to show where its route stops; that's only if it answered before the
//! outage, so the unused addresses of a subnet get none.
//!
//! Each round's routers and round trips also go into the host's
//! [`Paths`](crate::Paths), to show which way its traffic went at any time in the
//! last day.
//!
//! All of this needs network lookups on (see [`ProbeSettings`]); without them no
//! probes go out, and hosts have no way out.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::engine::{Event, EventKind, HostId, HostStatus, ProbeSettings, Shared, estimate_rtt_ms};
use crate::icmp::{self, EchoStatus};
use crate::network::{self, Owner};
use crate::now_secs;

/// How often each host's way out is checked.
pub const CHECK_EVERY: Duration = Duration::from_secs(30);
/// How often it's checked while the host is down after answering before.
pub const DOWN_CHECK_EVERY: Duration = Duration::from_secs(120);
/// How far out to look. The way out is normally within the first few hops.
pub(crate) const MAX_HOPS: u8 = 12;
/// Gap between a round's probes, so they don't reach the routers as a burst.
const SPACING: Duration = Duration::from_millis(20);
/// A router that skips an answer is assumed to still be there for this long.
const REMEMBER_SECS: f64 = 150.0;
/// Rounds in a row a different network has to show up before it counts as a switch.
const CONFIRM_ROUNDS: u32 = 2;
/// Switches kept per host, for marking the graph.
const MAX_CHANGES: usize = 500;

/// Where a host's traffic leaves for the internet: the first public network on its route.
#[derive(Clone, Debug, PartialEq)]
pub struct Exit {
    /// The network's AS number.
    pub asn: u32,
    /// The first router of that network that answered, and how many hops away it is.
    pub router: IpAddr,
    pub hop: u8,
    /// When traffic started leaving this way, or when FerrisPing first saw it.
    pub since: f64,
    /// When a probe last confirmed it.
    pub seen: f64,
}

/// A switch to another way out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExitChange {
    /// When the new network first showed up.
    pub at: f64,
    pub from: u32,
    pub to: u32,
}

/// One round of probes.
#[derive(Debug, Default)]
pub(crate) struct Round {
    /// The router that answered at each hop (index 0 is hop 1), if one did, and
    /// its round trip in milliseconds.
    pub(crate) routers: Vec<Option<(IpAddr, f32)>>,
    /// How many hops away the target itself answered, if it did, and its round trip.
    pub(crate) reached: Option<(u8, f32)>,
    /// The nearest hop where a router answered that the target can't be reached.
    pub(crate) unreachable: Option<u8>,
}

/// What a round shows about the way out.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Seen {
    /// The first public network, with its first router and that router's hop.
    Network { asn: u32, router: IpAddr, hop: u8 },
    /// No public network answered before the target.
    Nothing,
    /// A router's owner (or its network's name) isn't known yet, so this round can't tell.
    Unsure,
}

#[derive(Clone, Copy, Debug)]
struct Candidate {
    asn: u32,
    first_seen: f64,
    rounds: u32,
}

/// Works out a host's way out from round after round of probes.
#[derive(Debug, Default)]
struct Tracker {
    /// The router last heard at each hop (index 0 is hop 1), and when.
    hops: Vec<Option<(IpAddr, f64)>>,
    /// A different network that showed up lately, waiting to be confirmed.
    candidate: Option<Candidate>,
}

impl Tracker {
    /// Take in a round. Hops that stay quiet keep their router for a while, so a
    /// router that skips an answer now and then doesn't look like a new path.
    fn record(&mut self, round: &Round, now: f64) {
        let routers = round
            .reached
            .map_or(round.routers.len(), |(hop, _)| usize::from(hop).saturating_sub(1));
        self.hops.resize(routers, None);
        for (i, known) in self.hops.iter_mut().enumerate() {
            match round.routers.get(i).copied().flatten() {
                Some((addr, _)) => *known = Some((addr, now)),
                None => {
                    if known.is_some_and(|(_, heard)| now - heard > REMEMBER_SECS) {
                        *known = None;
                    }
                }
            }
        }
    }

    /// The routers on the way, nearest first, with their hops.
    fn routers(&self) -> Vec<(u8, IpAddr)> {
        self.hops
            .iter()
            .enumerate()
            .filter_map(|(i, known)| known.map(|(addr, _)| (i as u8 + 1, addr)))
            .collect()
    }

    /// Update `exit` with what a round showed, returning a confirmed switch.
    /// `same` tells whether two networks count as one (they share a name).
    fn observe(
        &mut self,
        exit: &mut Option<Exit>,
        seen: Seen,
        now: f64,
        same: impl Fn(u32, u32) -> bool,
    ) -> Option<ExitChange> {
        let Seen::Network { asn, router, hop } = seen else {
            // A round that shows nothing public, or can't tell, changes nothing.
            return None;
        };
        let Some(current) = exit.as_mut() else {
            *exit = Some(Exit {
                asn,
                router,
                hop,
                since: now,
                seen: now,
            });
            return None;
        };
        if same(current.asn, asn) {
            *current = Exit {
                asn,
                router,
                hop,
                since: current.since,
                seen: now,
            };
            self.candidate = None;
            return None;
        }
        let candidate = match self.candidate.take() {
            Some(c) if same(c.asn, asn) => Candidate {
                asn,
                rounds: c.rounds + 1,
                ..c
            },
            _ => Candidate {
                asn,
                first_seen: now,
                rounds: 1,
            },
        };
        if candidate.rounds < CONFIRM_ROUNDS {
            self.candidate = Some(candidate);
            return None;
        }
        let change = ExitChange {
            at: candidate.first_seen,
            from: current.asn,
            to: asn,
        };
        *current = Exit {
            asn,
            router,
            hop,
            since: candidate.first_seen,
            seen: now,
        };
        Some(change)
    }
}

/// The first public network along `routers` (nearest first, with their owners).
fn first_network(routers: &[(u8, IpAddr, Option<Owner>)]) -> Seen {
    for &(hop, router, owner) in routers {
        match owner {
            Some(Owner::Network(asn)) => return Seen::Network { asn, router, hop },
            Some(Owner::Private | Owner::Unannounced) => {}
            None => return Seen::Unsure,
        }
    }
    Seen::Nothing
}

/// Keep track of which network `target`'s traffic leaves through while network
/// lookups are on, until the host is gone or this task is dropped.
pub(crate) async fn watch(shared: Arc<Shared>, id: HostId, target: IpAddr) {
    let mut settings = shared.settings_tx.subscribe();
    // Each time lookups are turned on, start afresh: what was seen before is out of date.
    while settings.wait_for(|s| s.network_lookups).await.is_ok() {
        if !follow(&shared, id, target, &mut settings).await {
            return;
        }
    }
}

/// Check the way out round after round. Returns true when network lookups are
/// turned off, and false once the host is gone.
async fn follow(
    shared: &Arc<Shared>,
    id: HostId,
    target: IpAddr,
    settings: &mut watch::Receiver<ProbeSettings>,
) -> bool {
    let mut tracker = Tracker::default();
    let mut reached = None;
    // When this host's latest round while it was down began.
    let mut down_round: Option<Instant> = None;
    loop {
        let s = *settings.borrow_and_update();
        if !s.network_lookups {
            return true;
        }
        if s.paused {
            if settings.changed().await.is_err() {
                return false;
            }
            continue;
        }
        let started = Instant::now();
        let round = match turn_of(shared, id) {
            None => return false,
            Some(Turn::SitOut) => None,
            Some(Turn::Down) if down_round.is_some_and(|at| started - at < DOWN_CHECK_EVERY) => None,
            Some(turn) => {
                // One host at a time: routers limit how fast they report expired
                // probes, and hosts usually share the first few. Hosts that are down
                // queue apart, so their rounds, which wait out the whole timeout,
                // don't hold up the rest.
                let queue = match turn {
                    Turn::Down => &shared.down_turn,
                    _ => &shared.exit_turn,
                };
                let _turn = queue.lock().await;
                down_round = (turn == Turn::Down).then_some(started);
                match turn_of(shared, id) {
                    None => return false,
                    Some(Turn::SitOut) => None,
                    Some(_) => {
                        let limit = reached.unwrap_or(MAX_HOPS).min(MAX_HOPS);
                        let t = now_secs();
                        Some((t, probe(target, limit, s, shared.overhead_ms()).await))
                    }
                }
            }
        };
        if let Some((t, round)) = round {
            reached = round.reached.map(|(hop, _)| hop);
            if !record_path(shared, id, t, &round) {
                return false;
            }
            tracker.record(&round, now_secs());

            let mut routers = Vec::new();
            for (hop, router) in tracker.routers() {
                let owner = network::owner_of(shared, router).await;
                routers.push((hop, router, owner));
                if !matches!(owner, Some(Owner::Private | Owner::Unannounced)) {
                    break;
                }
            }
            if !update(shared, id, &mut tracker, first_network(&routers), now_secs()) {
                return false;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep_until(started + CHECK_EVERY) => {}
            off = settings.wait_for(|s| !s.network_lookups) => return off.is_ok(),
        }
    }
}

/// How a host takes its turns at checking the way out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Turn {
    /// With the other hosts, every [`CHECK_EVERY`].
    Up,
    /// Down after answering before: every [`DOWN_CHECK_EVERY`], in a queue of its own.
    /// A round to a host that doesn't answer lasts the whole timeout, and in the
    /// other queue would hold up every host behind it.
    Down,
    /// Down without ever having answered, like an unused address in a subnet: it
    /// sits out.
    SitOut,
}

/// How the host takes its next turn, or None once it's gone.
fn turn_of(shared: &Shared, id: HostId) -> Option<Turn> {
    let state = shared.read();
    let host = state.host(id)?;
    Some(match host.status {
        HostStatus::Down if host.was_up => Turn::Down,
        HostStatus::Down => Turn::SitOut,
        _ => Turn::Up,
    })
}

/// Add a round made at `t` to the host's paths. Returns false once the host is gone.
fn record_path(shared: &Shared, id: HostId, t: f64, round: &Round) -> bool {
    let mut guard = shared.write();
    let state = &mut *guard;
    let Some(host) = state.hosts.iter_mut().find(|h| h.id == id) else {
        return false;
    };
    // Turned off while this round was out: what it saw is kept no more than the rest.
    if !state.settings.network_lookups {
        return true;
    }
    host.paths.record(t, round);
    host.version += 1;
    drop(guard);
    shared.notify();
    true
}

/// Apply a round's result to the host. Returns false once the host is gone.
fn update(shared: &Shared, id: HostId, tracker: &mut Tracker, seen: Seen, now: f64) -> bool {
    let mut guard = shared.write();
    let state = &mut *guard;
    let Some(host) = state.hosts.iter_mut().find(|h| h.id == id) else {
        return false;
    };
    // Turned off while this round was out: the host has no way out until they're back on.
    if !state.settings.network_lookups {
        return true;
    }
    let networks = &state.networks;
    // Networks are told apart by name, so wait until the new one has its name.
    let seen = match seen {
        Seen::Network { asn, .. } if !networks.name_known(asn) => Seen::Unsure,
        seen => seen,
    };
    let change = tracker.observe(&mut host.exit, seen, now, |a, b| networks.name(a) == networks.name(b));
    if let Some(change) = change {
        host.exit_changes.push(change);
        if host.exit_changes.len() > MAX_CHANGES {
            host.exit_changes.remove(0);
        }
        let event = Event {
            at: change.at,
            host: id,
            host_name: host.name().to_string(),
            host_address: host.address(),
            kind: EventKind::ExitChanged {
                from: change.from,
                to: change.to,
            },
        };
        shared.log(state, event);
    }
    drop(guard);
    shared.notify();
    true
}

/// Send one probe to each hop up to `limit`, a few milliseconds apart, and wait for them all.
async fn probe(target: IpAddr, limit: u8, s: ProbeSettings, overhead_ms: f64) -> Round {
    let mut probes = JoinSet::new();
    for ttl in 1..=limit {
        let (timeout, payload_len) = (s.timeout, s.payload_len);
        probes.spawn_blocking(move || (ttl, icmp::echo(target, timeout, ttl, payload_len)));
        tokio::time::sleep(SPACING).await;
    }
    let mut round = Round {
        routers: vec![None; usize::from(limit)],
        ..Round::default()
    };
    while let Some(done) = probes.join_next().await {
        let Ok((ttl, reply)) = done else { continue };
        let ms = estimate_rtt_ms(&reply, overhead_ms);
        match reply.status {
            EchoStatus::Reply => {
                if round.reached.is_none_or(|(hop, _)| ttl < hop) {
                    round.reached = Some((ttl, ms));
                }
            }
            EchoStatus::TtlExpired | EchoStatus::Unreachable => {
                round.routers[usize::from(ttl) - 1] = reply.from.map(|from| (from, ms));
                if reply.status == EchoStatus::Unreachable && round.unreachable.is_none_or(|hop| ttl < hop) {
                    round.unreachable = Some(ttl);
                }
            }
            EchoStatus::TimedOut | EchoStatus::Failed(_) => {}
        }
    }
    round
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TargetSpec;
    use crate::engine::Host;

    const LUMEN: u32 = 3356;
    const LUMEN_LEGACY: u32 = 209;
    const COMCAST: u32 = 7922;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([63, 0, 0, n])
    }

    fn seen(asn: u32, hop: u8) -> Seen {
        Seen::Network {
            asn,
            router: ip(hop),
            hop,
        }
    }

    /// Lumen's two AS numbers are one ISP.
    fn same(a: u32, b: u32) -> bool {
        let isp = |asn| if asn == LUMEN_LEGACY { LUMEN } else { asn };
        isp(a) == isp(b)
    }

    #[test]
    fn the_first_public_network_is_the_way_out() {
        let private = IpAddr::from([10, 0, 0, 1]);
        let routers = [
            (1, private, Some(Owner::Private)),
            (2, ip(2), Some(Owner::Unannounced)),
            (3, ip(3), Some(Owner::Network(LUMEN))),
            (4, ip(4), Some(Owner::Network(COMCAST))),
        ];
        assert_eq!(first_network(&routers), seen(LUMEN, 3));
        assert_eq!(first_network(&routers[..2]), Seen::Nothing);
        // An owner that isn't known yet could be the way out itself.
        let unsure = [
            (1, private, Some(Owner::Private)),
            (2, ip(2), None),
            (3, ip(3), Some(Owner::Network(LUMEN))),
        ];
        assert_eq!(first_network(&unsure), Seen::Unsure);
    }

    #[test]
    fn the_first_sighting_is_not_a_switch() {
        let mut tracker = Tracker::default();
        let mut exit = None;
        assert_eq!(tracker.observe(&mut exit, Seen::Nothing, 0.0, same), None);
        assert_eq!(exit, None);
        assert_eq!(tracker.observe(&mut exit, seen(LUMEN, 3), 10.0, same), None);
        let exit = exit.unwrap();
        assert_eq!((exit.asn, exit.hop, exit.since, exit.seen), (LUMEN, 3, 10.0, 10.0));
    }

    #[test]
    fn a_switch_counts_after_two_rounds_and_dates_from_the_first() {
        let mut tracker = Tracker::default();
        let mut exit = None;
        tracker.observe(&mut exit, seen(LUMEN, 3), 0.0, same);
        assert_eq!(tracker.observe(&mut exit, seen(COMCAST, 3), 30.0, same), None);
        assert_eq!(exit.as_ref().unwrap().asn, LUMEN, "one round isn't enough");
        // A round that can't tell neither confirms nor cancels it.
        assert_eq!(tracker.observe(&mut exit, Seen::Unsure, 60.0, same), None);
        let change = tracker.observe(&mut exit, seen(COMCAST, 3), 90.0, same);
        assert_eq!(
            change,
            Some(ExitChange {
                at: 30.0,
                from: LUMEN,
                to: COMCAST
            })
        );
        let exit = exit.unwrap();
        assert_eq!((exit.asn, exit.since, exit.seen), (COMCAST, 30.0, 90.0));
    }

    #[test]
    fn going_back_cancels_a_switch() {
        let mut tracker = Tracker::default();
        let mut exit = None;
        tracker.observe(&mut exit, seen(LUMEN, 3), 0.0, same);
        tracker.observe(&mut exit, seen(COMCAST, 3), 30.0, same);
        tracker.observe(&mut exit, seen(LUMEN, 3), 60.0, same);
        assert_eq!(tracker.observe(&mut exit, seen(COMCAST, 3), 90.0, same), None);
        assert_eq!(exit.unwrap().asn, LUMEN);
    }

    #[test]
    fn another_number_for_the_same_isp_is_not_a_switch() {
        let mut tracker = Tracker::default();
        let mut exit = None;
        tracker.observe(&mut exit, seen(LUMEN, 3), 0.0, same);
        for t in [30.0, 60.0, 90.0] {
            assert_eq!(tracker.observe(&mut exit, seen(LUMEN_LEGACY, 4), t, same), None);
        }
        let exit = exit.unwrap();
        assert_eq!(
            (exit.asn, exit.hop, exit.since, exit.seen),
            (LUMEN_LEGACY, 4, 0.0, 90.0)
        );
    }

    #[test]
    fn rounds_that_end_after_lookups_are_turned_off_change_nothing() {
        let shared = Shared::new(ProbeSettings::default());
        let id = HostId(1);
        {
            let mut state = shared.write();
            state.hosts.push(Host::new(id, TargetSpec::new("10.0.0.1"), 0.0));
            state
                .networks
                .insert_for_test(ip(3), LUMEN, "LEVEL3 - Level 3 Parent, LLC, US");
            state
                .networks
                .insert_for_test(ip(4), COMCAST, "COMCAST-7922 - Comcast Cable Communications, LLC, US");
        }
        let mut tracker = Tracker::default();
        assert!(update(&shared, id, &mut tracker, seen(LUMEN, 3), 0.0));
        shared.write().settings.network_lookups = false;
        // Two rounds would make a switch, but they ended after lookups were turned off.
        for t in [30.0, 60.0] {
            assert!(update(&shared, id, &mut tracker, seen(COMCAST, 4), t));
        }
        let state = shared.read();
        assert_eq!(state.hosts[0].exit.as_ref().map(|exit| exit.asn), Some(LUMEN));
        assert!(state.events.is_empty(), "no switch is logged");
    }

    #[test]
    fn rounds_go_into_the_paths_while_lookups_are_on() {
        let shared = Shared::new(ProbeSettings::default());
        let id = HostId(1);
        shared
            .write()
            .hosts
            .push(Host::new(id, TargetSpec::new("192.0.2.1"), 0.0));
        let round = Round {
            routers: vec![Some((IpAddr::from([203, 0, 113, 1]), 1.5))],
            reached: Some((2, 9.0)),
            unreachable: None,
        };
        let version = shared.read().hosts[0].version;
        assert!(record_path(&shared, id, 30.0, &round));
        {
            let state = shared.read();
            let host = &state.hosts[0];
            assert!(host.version > version, "the UI sees the change");
            assert_eq!(host.paths.current().map(|path| path.checks), Some(1));
        }
        shared.write().settings.network_lookups = false;
        assert!(record_path(&shared, id, 60.0, &round));
        assert_eq!(shared.read().hosts[0].paths.current().map(|path| path.checks), Some(1));
        assert!(!record_path(&shared, HostId(2), 90.0, &round), "a host that's gone");
    }

    #[test]
    fn hosts_that_are_down_take_turns_apart_or_sit_out() {
        let shared = Shared::new(ProbeSettings::default());
        let id = HostId(1);
        shared
            .write()
            .hosts
            .push(Host::new(id, TargetSpec::new("192.0.2.1"), 0.0));
        for (status, was_up, turn) in [
            (HostStatus::Waiting, false, Turn::Up),
            (HostStatus::Up, false, Turn::Up),
            (HostStatus::Down, true, Turn::Down),
            // Like an unused address in a subnet.
            (HostStatus::Down, false, Turn::SitOut),
        ] {
            {
                let mut state = shared.write();
                state.hosts[0].status = status;
                state.hosts[0].was_up = was_up;
            }
            assert_eq!(turn_of(&shared, id), Some(turn), "{status:?}, was up: {was_up}");
        }
        assert_eq!(turn_of(&shared, HostId(2)), None, "a host that's gone");
    }

    #[test]
    fn quiet_routers_are_remembered_for_a_while() {
        let mut tracker = Tracker::default();
        let round = |routers: &[Option<u8>], reached: Option<u8>| Round {
            routers: routers.iter().map(|n| n.map(|n| (ip(n), 1.0))).collect(),
            reached: reached.map(|hop| (hop, 1.0)),
            unreachable: None,
        };
        tracker.record(&round(&[Some(1), Some(2), Some(3), Some(9)], None), 0.0);
        assert_eq!(tracker.routers(), [(1, ip(1)), (2, ip(2)), (3, ip(3)), (4, ip(9))]);

        // Hop 2 skips an answer; the target now answers at hop 4.
        tracker.record(&round(&[Some(1), None, Some(3), None], Some(4)), 30.0);
        assert_eq!(tracker.routers(), [(1, ip(1)), (2, ip(2)), (3, ip(3))]);

        // Long enough quiet and it's forgotten.
        tracker.record(
            &round(&[Some(1), None, Some(3), None], Some(4)),
            30.0 + REMEMBER_SECS + 1.0,
        );
        assert_eq!(tracker.routers(), [(1, ip(1)), (3, ip(3))]);

        // A different router at a hop replaces the old one at once.
        tracker.record(&round(&[Some(1), Some(7), Some(8), None], Some(4)), 400.0);
        assert_eq!(tracker.routers(), [(1, ip(1)), (2, ip(7)), (3, ip(8))]);

        // A target on the same network: no routers in between.
        tracker.record(&round(&[None], Some(1)), 430.0);
        assert!(tracker.routers().is_empty());
    }

    /// Probes and looks up for real:
    /// `cargo test -p ferrisping-core -- --ignored live --nocapture`.
    #[test]
    #[ignore = "needs the internet"]
    fn live_way_out() {
        let monitor = crate::Monitor::new(crate::ProbeSettings::default());
        monitor.add(crate::TargetSpec::new("1.1.1.1"));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            {
                let state = monitor.read();
                if let Some(exit) = &state.hosts[0].exit {
                    let name = state.networks.name(exit.asn);
                    println!(
                        "1.1.1.1 leaves via {name} (AS{}): {} at hop {}",
                        exit.asn, exit.router, exit.hop
                    );
                    return;
                }
            }
            assert!(std::time::Instant::now() < deadline, "no way out found");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
