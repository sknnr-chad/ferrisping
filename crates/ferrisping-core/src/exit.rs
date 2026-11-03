//! Which network each host's traffic leaves through, and when that changes.
//!
//! Every half minute each host that isn't down gets a light, partial traceroute: one
//! probe per hop, a dozen hops out at most. The first router on a public network
//! marks the way out: the ISP of a home or office line, or whichever of several
//! circuits BGP picked. A different network there two rounds in a row counts as a
//! switch.
//!
//! All of this needs network lookups on (see [`ProbeSettings`]); without them no
//! probes go out, and hosts have no way out.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::engine::{Event, EventKind, HostId, HostStatus, ProbeSettings, Shared};
use crate::icmp::{self, EchoStatus};
use crate::network::{self, Owner};
use crate::now_secs;

/// How often each host's way out is checked.
pub const CHECK_EVERY: Duration = Duration::from_secs(30);
/// How far out to look. The way out is normally within the first few hops.
const MAX_HOPS: u8 = 12;
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
struct Round {
    /// The router that answered at each hop (index 0 is hop 1), if one did.
    routers: Vec<Option<IpAddr>>,
    /// How many hops away the target itself answered, if it did.
    reached: Option<u8>,
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
            .map_or(round.routers.len(), |hop| usize::from(hop).saturating_sub(1));
        self.hops.resize(routers, None);
        for (i, known) in self.hops.iter_mut().enumerate() {
            match round.routers.get(i).copied().flatten() {
                Some(addr) => *known = Some((addr, now)),
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
        let round = {
            // One host at a time: routers limit how fast they report expired probes,
            // and hosts usually share the first few.
            let _turn = shared.exit_turn.lock().await;
            match takes_turn(shared, id) {
                None => return false,
                Some(false) => None,
                Some(true) => {
                    let limit = reached.unwrap_or(MAX_HOPS).min(MAX_HOPS);
                    Some(probe(target, limit, s.timeout, s.payload_len).await)
                }
            }
        };
        if let Some(round) = round {
            reached = round.reached;
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

/// Whether the host takes its turn at checking the way out, or None once it's gone.
/// A host that's down sits it out: a round to a host that doesn't answer lasts the
/// whole timeout while every other host waits, so a subnet's unused addresses would
/// leave the rest checked only every few minutes.
fn takes_turn(shared: &Shared, id: HostId) -> Option<bool> {
    let state = shared.read();
    Some(state.host(id)?.status != HostStatus::Down)
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
async fn probe(target: IpAddr, limit: u8, timeout: Duration, payload_len: u16) -> Round {
    let mut probes = JoinSet::new();
    for ttl in 1..=limit {
        probes.spawn_blocking(move || (ttl, icmp::echo(target, timeout, ttl, payload_len)));
        tokio::time::sleep(SPACING).await;
    }
    let mut round = Round {
        routers: vec![None; usize::from(limit)],
        reached: None,
    };
    while let Some(done) = probes.join_next().await {
        let Ok((ttl, reply)) = done else { continue };
        match reply.status {
            EchoStatus::Reply => round.reached = Some(round.reached.map_or(ttl, |hop| hop.min(ttl))),
            EchoStatus::TtlExpired | EchoStatus::Unreachable => round.routers[usize::from(ttl) - 1] = reply.from,
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
    fn hosts_that_are_down_sit_out_their_turn() {
        let shared = Shared::new(ProbeSettings::default());
        let id = HostId(1);
        shared
            .write()
            .hosts
            .push(Host::new(id, TargetSpec::new("10.0.0.1"), 0.0));
        for (status, turn) in [
            (HostStatus::Waiting, true),
            (HostStatus::Up, true),
            (HostStatus::Down, false),
        ] {
            shared.write().hosts[0].status = status;
            assert_eq!(takes_turn(&shared, id), Some(turn), "{status:?}");
        }
        assert_eq!(takes_turn(&shared, HostId(2)), None, "a host that's gone");
    }

    #[test]
    fn quiet_routers_are_remembered_for_a_while() {
        let mut tracker = Tracker::default();
        let round = |routers: &[Option<u8>], reached| Round {
            routers: routers.iter().map(|n| n.map(ip)).collect(),
            reached,
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
