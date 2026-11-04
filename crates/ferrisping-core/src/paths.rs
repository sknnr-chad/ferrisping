//! The paths each host's traffic took, as the route checks saw them.
//!
//! Each route check (see [`crate::exit`]) traces the first hops to a host, one probe
//! a hop. A host's routers are kept as a path, and another path is kept only when
//! its routers differ from the one before, each with when it was in use: then a
//! stretch of high latency can be held up against a change of route. Every check's
//! round trip at each hop is kept as well, in two bytes, to show where extra delay
//! starts.
//!
//! Load balancing has routers take turns at a hop. So a router that's new at a hop
//! only makes another path once it has answered there in [`CONFIRM`] checks while
//! none of the path's own routers did; when one of them does, the new router becomes
//! one of the path's own. A router can sit out more turns than that, so for a while
//! after a change, a new path that turns out to share its routers with the one
//! before is folded back into it.

use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;

use crate::exit::{MAX_HOPS, Round};

/// Checks a new router has to answer at a hop in, while none of the path's own
/// routers answers there, before the path counts as changed.
const CONFIRM: u32 = 4;
/// A new router that hasn't made another path after this many checks becomes one of
/// the path's own: it answers too seldom to tell.
const GIVE_UP: u32 = 3 * CONFIRM;
/// How long after a change a new path that turns out to share its routers with the
/// one before is folded back into it.
const FOLD_BACK_SECS: f64 = 600.0;
/// How long a path counts as still in use after a check saw it, when no check since
/// says otherwise. Checks come every 30 seconds, or less often when many hosts take
/// turns.
pub const IN_USE_SECS: f64 = 180.0;
/// Paths kept per host, however often its route changes.
const MAX_PATHS: usize = 500;
/// Checks kept per host: a little over a day's worth at one every 30 seconds.
const MAX_CHECKS: usize = 3200;
/// What a check's path is while it's not part of one yet.
const UNSETTLED: u32 = u32::MAX;
const HOPS: usize = MAX_HOPS as usize;

/// Who answered at a hop, in two bytes: nobody, the host itself, or a router by its
/// place in [`Paths::routers`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Who(u16);

impl Who {
    const NOBODY: Self = Self(0);
    const HOST: Self = Self(1);
    /// Routers past this many share [`Who::NOBODY`]; a host never sees that many.
    const MAX_ROUTERS: usize = (u16::MAX - 2) as usize;

    fn router(index: usize) -> Self {
        Self(index as u16 + 2)
    }

    fn router_index(self) -> Option<usize> {
        self.0.checked_sub(2).map(usize::from)
    }
}

/// A round trip in tenths of a millisecond, in two bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rtt(u16);

impl Rtt {
    const NONE: Self = Self(u16::MAX);

    fn new(ms: f32) -> Self {
        Self((ms * 10.0).round().clamp(0.0, f32::from(u16::MAX - 1)) as u16)
    }

    fn ms(self) -> Option<f32> {
        (self != Self::NONE).then(|| f32::from(self.0) / 10.0)
    }
}

/// One route check, as kept.
#[derive(Clone, Copy, Debug)]
struct Check {
    t: f64,
    /// The path it's part of, by id, or [`UNSETTLED`].
    path: u32,
    /// How many hops it saw: up to the host when that answered, else as far as it probed.
    len: u8,
    who: [Who; HOPS],
    rtt: [Rtt; HOPS],
}

impl Check {
    /// Whether the host itself answered, at the check's last hop.
    fn reached(&self) -> bool {
        self.len > 0 && self.who[usize::from(self.len) - 1] == Who::HOST
    }
}

/// One path to a host: who answered at each hop while it was in use.
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    id: u32,
    /// Hop 1, the nearest, first.
    pub hops: Vec<PathHop>,
    /// The first and the last check that saw it, in seconds since the UNIX epoch.
    pub first: f64,
    pub last: f64,
    /// How many checks saw it.
    pub checks: u32,
    /// Hops (counted from 1) where it differs from the path before it: no router in
    /// common, or one of the two goes further. Empty for the first path seen.
    pub changed: Vec<u8>,
}

/// Who answered at one hop of a path.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathHop {
    /// The routers that answered here, the most checks first. Several take turns
    /// where traffic is load-balanced; none answers at a silent hop.
    pub routers: Vec<RouterSeen>,
    /// How many checks the host itself answered in at this hop, where the path ends.
    pub host: u32,
}

/// A router that answered at a hop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RouterSeen {
    pub addr: IpAddr,
    /// How many checks it answered in there.
    pub checks: u32,
}

impl PathHop {
    /// Nobody answered here.
    pub fn is_silent(&self) -> bool {
        self.routers.is_empty() && self.host == 0
    }

    fn count(&mut self, who: Who, routers: &[IpAddr]) {
        match who.router_index() {
            Some(index) => tally(&mut self.routers, routers[index], 1),
            None if who == Who::HOST => self.host += 1,
            None => {}
        }
    }

    fn set(&self) -> HopSet {
        HopSet {
            routers: self.routers.iter().map(|r| r.addr).collect(),
            host: self.host > 0,
        }
    }
}

impl Path {
    fn new(id: u32) -> Self {
        Self {
            id,
            hops: Vec::new(),
            first: 0.0,
            last: 0.0,
            checks: 0,
            changed: Vec::new(),
        }
    }

    /// The hops up to the last one where someone answered.
    pub fn shown_hops(&self) -> &[PathHop] {
        let end = self.hops.iter().rposition(|hop| !hop.is_silent()).map_or(0, |i| i + 1);
        &self.hops[..end]
    }

    /// Whether the host itself answered at the end of it.
    pub fn reaches_host(&self) -> bool {
        self.hops.iter().any(|hop| hop.host > 0)
    }

    /// Whether `other` is the same route: no hop of either differs from the other's.
    pub fn same_route(&self, other: &Path) -> bool {
        let (mine, theirs) = (self.sets(), other.sets());
        differs(&mine, &theirs).is_empty() && differs(&theirs, &mine).is_empty()
    }

    fn sets(&self) -> Vec<HopSet> {
        self.hops.iter().map(PathHop::set).collect()
    }

    /// Count a check in.
    fn take(&mut self, check: &Check, routers: &[IpAddr]) {
        if self.checks == 0 {
            self.first = check.t;
        }
        self.last = self.last.max(check.t);
        self.checks += 1;
        let len = usize::from(check.len);
        if self.hops.len() < len {
            self.hops.resize_with(len, PathHop::default);
        }
        for (hop, &who) in self.hops.iter_mut().zip(&check.who[..len]) {
            hop.count(who, routers);
        }
    }

    /// Take in another path's checks, as if they had seen this one.
    fn absorb(&mut self, other: Path) {
        if self.hops.len() < other.hops.len() {
            self.hops.resize_with(other.hops.len(), PathHop::default);
        }
        for (mine, theirs) in self.hops.iter_mut().zip(other.hops) {
            mine.host += theirs.host;
            for seen in theirs.routers {
                tally(&mut mine.routers, seen.addr, seen.checks);
            }
        }
        self.first = self.first.min(other.first);
        self.last = self.last.max(other.last);
        self.checks += other.checks;
    }
}

/// Who answered at a hop, to compare paths by.
#[derive(Clone, Debug, Default, PartialEq)]
struct HopSet {
    routers: Vec<IpAddr>,
    host: bool,
}

impl HopSet {
    fn is_empty(&self) -> bool {
        self.routers.is_empty() && !self.host
    }

    /// Whether someone answered at both.
    fn meets(&self, other: &HopSet) -> bool {
        (self.host && other.host) || self.routers.iter().any(|r| other.routers.contains(r))
    }
}

/// The hops (counted from 1) of path `b` that differ from path `a`: where both had
/// answers but none in common, and past where `a` ends at the host, unless the two
/// can end at the same hop (load balancing over ways of two lengths). A silent hop
/// differs from nothing.
fn differs(a: &[HopSet], b: &[HopSet]) -> Vec<u8> {
    let a_ends = a.last().is_some_and(|hop| hop.host);
    let common_end = a.iter().zip(b).any(|(a, b)| a.host && b.host);
    (0..b.len())
        .filter(|&k| match a.get(k) {
            Some(theirs) => !theirs.is_empty() && !b[k].is_empty() && !theirs.meets(&b[k]),
            None => a_ends && !common_end && !b[k].is_empty(),
        })
        .map(|k| k as u8 + 1)
        .collect()
}

/// Routers new to a hop of the path in use: load balancing, or another path.
#[derive(Clone, Debug)]
struct Fresh {
    who: Vec<Who>,
    /// When the first of them answered.
    since: f64,
    /// Checks since then where one of them answered here, and checks in all.
    answered: u32,
    checks: u32,
}

/// Every path a host's traffic took over the last day or so, and the checks that
/// saw them.
#[derive(Debug, Default)]
pub struct Paths {
    /// Oldest first. The last is the path in use, as far as the checks know.
    paths: VecDeque<Path>,
    /// Oldest first. The newest may not be part of a path yet.
    checks: VecDeque<Check>,
    /// The routers that answered, so each check keeps two bytes a hop.
    routers: Vec<IpAddr>,
    router_index: HashMap<IpAddr, u16>,
    /// How many of the newest checks aren't part of a path yet.
    unsettled: usize,
    /// At each hop of the path in use, who answered there that isn't one of its own.
    fresh: Vec<Option<Fresh>>,
    /// At each hop of the path in use, who became one of its own from checks that
    /// aren't part of it yet.
    taken_in: Vec<Vec<Who>>,
    /// Until when the path in use can still be folded back into the one before.
    fold_back_until: Option<f64>,
    next_id: u32,
    version: u64,
}

impl Paths {
    /// The paths kept, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Path> + ExactSizeIterator {
        self.paths.iter()
    }

    /// The path in use, as far as the checks know.
    pub fn current(&self) -> Option<&Path> {
        self.paths.back()
    }

    /// The paths that came with a change of route, oldest first: each started at its
    /// `first`, and its `changed` hops say what changed.
    pub fn changes(&self) -> impl Iterator<Item = &Path> {
        self.paths.iter().filter(|path| !path.changed.is_empty())
    }

    /// When the oldest check that isn't part of a path yet was made. Some router in
    /// it is new, and the next few checks tell whether the route changed.
    pub fn unsettled_since(&self) -> Option<f64> {
        let start = self.checks.len() - self.unsettled;
        self.checks.get(start).map(|check| check.t)
    }

    /// Bumped on every change, so the UI can cache what it derives.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The paths in use between `t0` and `t1`, oldest first, with what the checks in
    /// that time saw of each. The first may be the path in use as that time began,
    /// seen by a check a little before. Checks that aren't part of a path yet are
    /// left out.
    pub fn during(&self, t0: f64, t1: f64) -> Vec<PathSpan<'_>> {
        let settled = self.checks.len() - self.unsettled;
        let checks = self.checks.range(..settled);
        let start = self.checks.partition_point(|c| c.t < t0).min(settled);
        let end = self.checks.partition_point(|c| c.t <= t1).clamp(start, settled);
        let mut spans: Vec<PathSpan<'_>> = Vec::new();
        if let Some(before) = start.checked_sub(1).map(|i| &self.checks[i])
            && t0 - before.t <= IN_USE_SECS
            && let Some(path) = self.path(before.path)
        {
            spans.push(PathSpan::new(path));
        }
        let mut rtts: Vec<Vec<Vec<f32>>> = Vec::new();
        for check in checks.skip(start).take(end - start) {
            if spans.last().is_none_or(|span| span.path.id != check.path) {
                let Some(path) = self.path(check.path) else { continue };
                spans.push(PathSpan::new(path));
            }
            if rtts.len() < spans.len() {
                rtts.resize_with(spans.len(), Vec::new);
            }
            let span = spans.last_mut().expect("just pushed");
            span.count(check, &self.routers, rtts.last_mut().expect("just resized"));
        }
        for (i, span) in spans.iter_mut().enumerate() {
            if let Some(rtts) = rtts.get_mut(i) {
                for (hop, rtts) in span.hops.iter_mut().zip(rtts) {
                    hop.ms = median(rtts);
                }
            }
            let usual = self.usual_ms(span.path, t0, t1);
            for (hop, usual) in span.hops.iter_mut().zip(usual) {
                hop.usual_ms = usual;
            }
        }
        spans
    }

    /// The middle round trip at each hop of `path`, over its checks outside `t0..=t1`.
    fn usual_ms(&self, path: &Path, t0: f64, t1: f64) -> Vec<Option<f32>> {
        let settled = self.checks.len() - self.unsettled;
        let start = self.checks.partition_point(|c| c.t < path.first).min(settled);
        let end = self.checks.partition_point(|c| c.t <= path.last).clamp(start, settled);
        let mut rtts: Vec<Vec<f32>> = vec![Vec::new(); path.hops.len()];
        for check in self.checks.range(start..end) {
            if check.path != path.id || (t0..=t1).contains(&check.t) {
                continue;
            }
            for (rtts, rtt) in rtts.iter_mut().zip(&check.rtt[..usize::from(check.len)]) {
                rtts.extend(rtt.ms());
            }
        }
        rtts.iter_mut().map(|rtts| median(rtts)).collect()
    }

    fn path(&self, id: u32) -> Option<&Path> {
        let first = self.paths.front()?.id;
        self.paths.get(id.checked_sub(first)? as usize)
    }

    /// Take in a route check made at `t`.
    pub(crate) fn record(&mut self, t: f64, round: &Round) {
        let check = self.check(t, round);
        self.checks.push_back(check);
        self.unsettled += 1;
        self.version += 1;
        if self.paths.is_empty() {
            self.start_path();
        } else {
            self.weigh(&check);
            match self.confirmed() {
                Some(since) => self.change(since, t),
                None => self.settle_if_clear(),
            }
            self.reconsider(t);
        }
        self.cap();
    }

    /// Forget checks made before `t`, and paths no check has seen since.
    pub(crate) fn trim(&mut self, t: f64) {
        let before = (self.checks.len(), self.paths.len());
        while self.checks.len() > self.unsettled && self.checks.front().is_some_and(|c| c.t < t) {
            self.checks.pop_front();
        }
        while self.paths.len() > 1 && self.paths.front().is_some_and(|p| p.last < t) {
            self.paths.pop_front();
        }
        // The path in use goes too once nothing has seen it for that long, and with
        // it every router: no check is left to name one.
        if self.unsettled == 0 && self.paths.back().is_some_and(|p| p.last < t) {
            self.paths.clear();
            self.routers.clear();
            self.router_index.clear();
            self.forget_working();
        }
        if (self.checks.len(), self.paths.len()) != before {
            self.version += 1;
        }
    }

    /// Forget every path and check.
    pub(crate) fn clear(&mut self) {
        *self = Self {
            next_id: self.next_id,
            version: self.version + 1,
            ..Self::default()
        };
    }

    fn check(&mut self, t: f64, round: &Round) -> Check {
        let mut check = Check {
            t,
            path: UNSETTLED,
            len: 0,
            who: [Who::NOBODY; HOPS],
            rtt: [Rtt::NONE; HOPS],
        };
        let mut len = match round.reached {
            Some((hop, _)) => usize::from(hop),
            None => round.routers.len(),
        };
        // A router that says the host can't be reached. Unless it's the path's own
        // where it said so, it's most likely the router a hop nearer, which couldn't
        // hand the probe on: the one before a host that's down does that for every
        // probe that would have reached the host. So it goes in there, where nobody
        // else answers, or is left out, rather than look like a new router.
        let (mut left_out, mut moved) = (None, None);
        if let Some(hop) = round.unreachable {
            let k = usize::from(hop.max(1)) - 1;
            let reporter = round.routers.get(k).copied().flatten().map(|(addr, _)| addr);
            let own_at = |k| {
                reporter
                    .and_then(|addr| self.known(addr))
                    .is_some_and(|who| self.own(k, who))
            };
            if !own_at(k) {
                left_out = Some(k);
                if k > 0 && round.routers.get(k - 1) == Some(&None) && (self.silent(k - 1) || own_at(k - 1)) {
                    moved = reporter.map(|addr| (k - 1, addr));
                }
            }
            // Probes past it only reach it again.
            if round.reached.is_none() {
                len = len.min(k + usize::from(left_out.is_none()));
            }
        }
        let len = len.min(HOPS);
        for (k, answer) in round.routers.iter().enumerate().take(len) {
            if let Some((addr, ms)) = *answer
                && left_out != Some(k)
            {
                check.who[k] = self.intern(addr);
                check.rtt[k] = Rtt::new(ms);
            }
        }
        // Without its round trip: that waited on the host.
        if let Some((k, addr)) = moved
            && k < len
        {
            check.who[k] = self.intern(addr);
        }
        if let Some((hop, ms)) = round.reached
            && (1..=len).contains(&usize::from(hop))
        {
            let k = usize::from(hop) - 1;
            check.who[k] = Who::HOST;
            check.rtt[k] = Rtt::new(ms);
        }
        check.len = len as u8;
        check
    }

    fn intern(&mut self, addr: IpAddr) -> Who {
        if let Some(who) = self.known(addr) {
            return who;
        }
        if self.routers.len() >= Who::MAX_ROUTERS {
            return Who::NOBODY;
        }
        let index = self.routers.len();
        self.routers.push(addr);
        self.router_index.insert(addr, index as u16);
        Who::router(index)
    }

    /// A router that has answered before.
    fn known(&self, addr: IpAddr) -> Option<Who> {
        self.router_index
            .get(&addr)
            .map(|&index| Who::router(usize::from(index)))
    }

    /// Whether `who` is one of the path in use's own at hop `k`.
    fn own(&self, k: usize, who: Who) -> bool {
        if self.taken_in.get(k).is_some_and(|own| own.contains(&who)) {
            return true;
        }
        let Some(hop) = self.paths.back().and_then(|path| path.hops.get(k)) else {
            return false;
        };
        match who.router_index() {
            None => who == Who::HOST && hop.host > 0,
            Some(index) => hop.routers.iter().any(|r| r.addr == self.routers[index]),
        }
    }

    /// Whether the path in use goes as far as hop `k`, but nobody has answered there.
    fn silent(&self, k: usize) -> bool {
        self.paths
            .back()
            .and_then(|path| path.hops.get(k))
            .is_some_and(PathHop::is_silent)
            && self.taken_in.get(k).is_none_or(Vec::is_empty)
    }

    /// Weigh what a check saw against the path in use.
    fn weigh(&mut self, check: &Check) {
        let len = usize::from(check.len);
        if self.fresh.len() < len {
            self.fresh.resize(len, None);
        }
        if self.taken_in.len() < self.fresh.len() {
            self.taken_in.resize(self.fresh.len(), Vec::new());
        }
        for (k, &who) in check.who[..len].iter().enumerate() {
            if who == Who::NOBODY {
                continue;
            }
            if self.own(k, who) {
                // One of the path's own answered, so whoever was new here takes turns with it.
                self.take_in(k);
            } else if self.silent(k) {
                // Nobody answered here before, so as far as anyone can tell, this one was there all along.
                self.taken_in[k].push(who);
            } else {
                let fresh = self.fresh[k].get_or_insert_with(|| Fresh {
                    who: Vec::new(),
                    since: check.t,
                    answered: 0,
                    checks: 0,
                });
                if !fresh.who.contains(&who) {
                    fresh.who.push(who);
                }
                fresh.answered += 1;
            }
        }
        // The check ended at the host where the path in use can, so whoever was new
        // further out was on a longer way taking turns with this one.
        if check.reached() && self.own(len - 1, Who::HOST) {
            for k in len..self.fresh.len() {
                self.take_in(k);
            }
        }
        for k in 0..self.fresh.len() {
            if let Some(fresh) = &mut self.fresh[k] {
                fresh.checks += 1;
                if fresh.checks >= GIVE_UP && fresh.answered < CONFIRM {
                    self.take_in(k);
                }
            }
        }
    }

    /// Make whoever is new at hop `k` one of the path in use's own.
    fn take_in(&mut self, k: usize) {
        if let Some(fresh) = self.fresh.get_mut(k).and_then(Option::take) {
            let own = &mut self.taken_in[k];
            for who in fresh.who {
                if !own.contains(&who) {
                    own.push(who);
                }
            }
        }
    }

    /// When the route changed, once some hop says so.
    fn confirmed(&self) -> Option<f64> {
        self.fresh
            .iter()
            .flatten()
            .filter(|fresh| fresh.answered >= CONFIRM)
            .map(|fresh| fresh.since)
            .min_by(f64::total_cmp)
    }

    /// Make the oldest `count` unsettled checks part of the path at `index`.
    fn settle(&mut self, index: usize, count: usize) {
        let start = self.checks.len() - self.unsettled;
        let path = &mut self.paths[index];
        for check in self.checks.range_mut(start..start + count) {
            check.path = path.id;
            path.take(check, &self.routers);
        }
        self.unsettled -= count;
    }

    /// Once nothing new is waiting at any hop, the checks so far are part of the path in use.
    fn settle_if_clear(&mut self) {
        if self.fresh.iter().all(Option::is_none) {
            self.settle(self.paths.len() - 1, self.unsettled);
            self.taken_in.clear();
        }
    }

    fn start_path(&mut self) {
        self.paths.push_back(Path::new(self.next_id));
        self.next_id += 1;
        self.settle(self.paths.len() - 1, self.unsettled);
        self.forget_working();
    }

    /// The route changed at `since`, as the check at `now` confirms.
    fn change(&mut self, since: f64, now: f64) {
        let start = self.checks.len() - self.unsettled;
        let before = self.checks.range(start..).take_while(|c| c.t < since).count();
        let old = self.paths.len() - 1;
        self.settle(old, before);
        self.start_path();
        let new = &self.paths[old + 1];
        let changed = differs(&self.paths[old].sets(), &new.sets());
        self.paths[old + 1].changed = changed;
        self.fold_back_until = Some(now + FOLD_BACK_SECS);
    }

    /// While a change is recent, fold the new path back into the one before once
    /// their routers turn out to be the same after all.
    fn reconsider(&mut self, now: f64) {
        let Some(until) = self.fold_back_until else { return };
        let n = self.paths.len();
        if n < 2 {
            self.fold_back_until = None;
            return;
        }
        let before = self.paths[n - 2].sets();
        let mut current = self.paths[n - 1].sets();
        for (k, own) in self.taken_in.iter().enumerate() {
            if current.len() <= k {
                current.resize(k + 1, HopSet::default());
            }
            for &who in own {
                match who.router_index() {
                    Some(index) => current[k].routers.push(self.routers[index]),
                    None => current[k].host |= who == Who::HOST,
                }
            }
        }
        let changed = differs(&before, &current);
        if changed.is_empty() {
            self.fold_back();
            return;
        }
        self.paths[n - 1].changed = changed;
        if now >= until {
            self.fold_back_until = None;
        }
    }

    /// The path in use was the one before after all.
    fn fold_back(&mut self) {
        self.fold_back_until = None;
        let Some(current) = self.paths.pop_back() else { return };
        let Some(before) = self.paths.back_mut() else {
            self.paths.push_back(current);
            return;
        };
        let (from, to) = (current.id, before.id);
        before.absorb(current);
        for check in self.checks.iter_mut().rev() {
            if check.path == from {
                check.path = to;
            } else if check.path != UNSETTLED {
                break;
            }
        }
        self.next_id = from;
        // Some of whoever was new may be the path's own now.
        for k in 0..self.fresh.len() {
            let Some(mut fresh) = self.fresh[k].take() else {
                continue;
            };
            fresh.who.retain(|&who| !self.own(k, who));
            if !fresh.who.is_empty() {
                self.fresh[k] = Some(fresh);
            }
        }
        self.settle_if_clear();
    }

    fn forget_working(&mut self) {
        self.fresh.clear();
        self.taken_in.clear();
        self.fold_back_until = None;
    }

    /// Keep within the caps on paths and checks.
    fn cap(&mut self) {
        while self.paths.len() > MAX_PATHS {
            self.paths.pop_front();
        }
        let first = self.paths.front().map_or(0, |path| path.id);
        while self.checks.len() > self.unsettled
            && self
                .checks
                .front()
                .is_some_and(|c| self.checks.len() > MAX_CHECKS || c.path < first)
        {
            self.checks.pop_front();
        }
    }

    /// For UI tests: take in a check made at `t`, as `(router, ms)` for each hop
    /// from 1, and the host's round trip if it answered after the last of them.
    #[doc(hidden)]
    pub fn record_for_test(&mut self, t: f64, routers: &[Option<(IpAddr, f32)>], host_ms: Option<f32>) {
        let round = Round {
            routers: routers.to_vec(),
            reached: host_ms.map(|ms| (routers.len() as u8 + 1, ms)),
            unreachable: None,
        };
        self.record(t, &round);
    }
}

/// A path in use during a stretch of time, and what the checks in that time saw of it.
#[derive(Clone, Debug)]
pub struct PathSpan<'a> {
    pub path: &'a Path,
    /// The checks in that time that saw it, and when the first and last were made.
    /// None for the path in use as that time began, when the next check showed another.
    pub checks: u32,
    pub first: Option<f64>,
    pub last: Option<f64>,
    /// How many of those checks the host answered.
    pub reached: u32,
    /// Hop 1 first: who answered in those checks, and how fast.
    pub hops: Vec<SpanHop>,
}

/// One hop of a [`PathSpan`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpanHop {
    /// The routers that answered, the most checks first.
    pub routers: Vec<RouterSeen>,
    /// The checks the host itself answered in at this hop.
    pub host: u32,
    /// The middle round trip in those checks, in milliseconds.
    pub ms: Option<f32>,
    /// The middle round trip at this hop over the path's other checks, to compare with.
    pub usual_ms: Option<f32>,
}

impl<'a> PathSpan<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            checks: 0,
            first: None,
            last: None,
            reached: 0,
            hops: vec![SpanHop::default(); path.hops.len()],
        }
    }

    /// Whether the change to this path came in that time, rather than before.
    pub fn began_within(&self, t0: f64, t1: f64) -> bool {
        !self.path.changed.is_empty() && (t0..=t1).contains(&self.path.first)
    }

    fn count(&mut self, check: &Check, routers: &[IpAddr], rtts: &mut Vec<Vec<f32>>) {
        self.checks += 1;
        self.first.get_or_insert(check.t);
        self.last = Some(check.t);
        if check.reached() {
            self.reached += 1;
        }
        let len = usize::from(check.len);
        if self.hops.len() < len {
            self.hops.resize_with(len, SpanHop::default);
        }
        if rtts.len() < len {
            rtts.resize_with(len, Vec::new);
        }
        let answers = check.who.iter().zip(&check.rtt).take(len);
        for ((hop, rtts), (&who, rtt)) in self.hops.iter_mut().zip(rtts.iter_mut()).zip(answers) {
            match who.router_index() {
                Some(index) => tally(&mut hop.routers, routers[index], 1),
                None if who == Who::HOST => hop.host += 1,
                None => continue,
            }
            rtts.extend(rtt.ms());
        }
    }
}

/// Count `checks` more for the router at `addr`, keeping the most checks first.
fn tally(seen: &mut Vec<RouterSeen>, addr: IpAddr, checks: u32) {
    match seen.iter_mut().find(|r| r.addr == addr) {
        Some(known) => known.checks += checks,
        None => seen.push(RouterSeen { addr, checks }),
    }
    seen.sort_by_key(|r| Reverse(r.checks));
}

/// The middle value, or the mean of the two in the middle.
fn median(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f32::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: u8 = 0;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([198, 51, 100, n])
    }

    /// A check at `t` that saw these routers (by `ip`), 0 for the host itself, or
    /// `None` for no answer. Each hop takes `hop` × 10 ms.
    fn round(hops: &[Option<u8>]) -> Round {
        let mut round = Round::default();
        for (k, hop) in hops.iter().enumerate() {
            let ms = (k + 1) as f32 * 10.0;
            match *hop {
                Some(HOST) => {
                    round.reached = Some((k as u8 + 1, ms));
                    break;
                }
                Some(n) => round.routers.push(Some((ip(n), ms))),
                None => round.routers.push(None),
            }
        }
        round
    }

    /// Records `checks`, each 30 seconds after the one before, starting at `t`.
    fn record_all(paths: &mut Paths, t: f64, checks: &[&[Option<u8>]]) -> f64 {
        let mut t = t;
        for hops in checks {
            paths.record(t, &round(hops));
            t += 30.0;
        }
        t
    }

    /// Each path's routers by hop, the most checks first, with 0 for the host.
    fn routes(paths: &Paths) -> Vec<Vec<Vec<u8>>> {
        paths
            .iter()
            .map(|path| {
                path.hops
                    .iter()
                    .map(|hop| {
                        let mut who: Vec<u8> = hop
                            .routers
                            .iter()
                            .map(|r| match r.addr {
                                IpAddr::V4(v4) => v4.octets()[3],
                                IpAddr::V6(_) => unreachable!(),
                            })
                            .collect();
                        if hop.host > 0 {
                            who.push(HOST);
                        }
                        who
                    })
                    .collect()
            })
            .collect()
    }

    const A: &[Option<u8>] = &[Some(1), Some(2), Some(3), Some(HOST)];
    const B: &[Option<u8>] = &[Some(1), Some(7), Some(3), Some(HOST)];

    #[test]
    fn the_same_route_makes_one_path() {
        let mut paths = Paths::default();
        record_all(&mut paths, 1000.0, &[A, A, A]);
        assert_eq!(routes(&paths), [vec![vec![1], vec![2], vec![3], vec![HOST]]]);
        let path = paths.current().unwrap();
        assert_eq!((path.first, path.last, path.checks), (1000.0, 1060.0, 3));
        assert!(path.changed.is_empty(), "the first path changed nothing");
        assert!(path.reaches_host());
        assert_eq!(paths.changes().count(), 0);
    }

    #[test]
    fn a_new_router_makes_a_new_path_once_it_has_answered_four_times() {
        let mut paths = Paths::default();
        let t = record_all(&mut paths, 0.0, &[A, A, A]);
        record_all(&mut paths, t, &[B, B, B]);
        assert_eq!(paths.iter().count(), 1, "three checks aren't enough");
        assert_eq!(paths.unsettled_since(), Some(t));
        assert_eq!(paths.current().unwrap().last, t - 30.0, "the old path isn't seen since");

        record_all(&mut paths, t + 90.0, &[B]);
        assert_eq!(
            routes(&paths),
            [
                vec![vec![1], vec![2], vec![3], vec![HOST]],
                vec![vec![1], vec![7], vec![3], vec![HOST]],
            ]
        );
        let new = paths.current().unwrap();
        assert_eq!(
            (new.first, new.last, new.checks),
            (t, t + 90.0, 4),
            "it dates from its first check"
        );
        assert_eq!(new.changed, [2]);
        assert_eq!(paths.iter().next().unwrap().last, t - 30.0);
        assert_eq!(paths.unsettled_since(), None);
        assert_eq!(paths.changes().map(|p| p.first).collect::<Vec<_>>(), [t]);
    }

    #[test]
    fn routers_taking_turns_at_a_hop_are_one_path() {
        let mut paths = Paths::default();
        record_all(&mut paths, 0.0, &[A, B, A, A, B, B, A, B, B, B, A]);
        assert_eq!(routes(&paths), [vec![vec![1], vec![7, 2], vec![3], vec![HOST]]]);
        assert_eq!(paths.current().unwrap().checks, 11);
        assert_eq!(paths.current().unwrap().hops[1].routers[0].checks, 6);
    }

    #[test]
    fn a_router_that_waited_long_for_its_turn_is_folded_back() {
        let mut paths = Paths::default();
        // The second router takes its first turns four times in a row: that looks
        // like a change, until the first takes its turn again and then the second.
        let t = record_all(&mut paths, 0.0, &[A, A, B, B, B, B]);
        assert_eq!(paths.changes().count(), 1);
        record_all(&mut paths, t, &[A, B, A]);
        assert_eq!(routes(&paths), [vec![vec![1], vec![7, 2], vec![3], vec![HOST]]]);
        assert_eq!(paths.current().unwrap().checks, 9);
        assert_eq!(paths.changes().count(), 0);
    }

    #[test]
    fn going_back_after_a_while_is_a_change_too() {
        let mut paths = Paths::default();
        record_all(&mut paths, 0.0, &[A, A, B, B, B, B, B, B, A, A, A, A]);
        assert_eq!(paths.iter().count(), 3);
        let all: Vec<&Path> = paths.iter().collect();
        assert!(all[2].same_route(all[0]));
        assert!(!all[1].same_route(all[0]));
        assert_eq!(all[2].changed, [2]);
        assert_eq!((all[1].first, all[2].first), (60.0, 240.0));
    }

    #[test]
    fn a_change_stays_once_it_has_lasted() {
        let mut paths = Paths::default();
        let t = record_all(&mut paths, 0.0, &[A, A, B, B, B, B]);
        // Twenty minutes later, both take turns: the change stands.
        let mut checks = vec![B; 40];
        checks.extend([A, B, A]);
        record_all(&mut paths, t, &checks);
        assert_eq!(paths.iter().count(), 2);
        assert_eq!(paths.current().unwrap().changed, [2]);
        assert_eq!(routes(&paths)[1][1], [7, 2]);
    }

    #[test]
    fn a_longer_or_shorter_route_is_a_change() {
        let longer: &[Option<u8>] = &[Some(1), Some(2), Some(3), Some(4), Some(HOST)];
        let mut paths = Paths::default();
        record_all(&mut paths, 0.0, &[A, A, longer, longer, longer, longer]);
        assert_eq!(paths.iter().count(), 2);
        assert_eq!(paths.current().unwrap().changed, [4, 5]);

        let shorter: &[Option<u8>] = &[Some(1), Some(2), Some(HOST)];
        record_all(&mut paths, 1000.0, &[shorter; 4]);
        assert_eq!(paths.iter().count(), 3);
        assert_eq!(paths.current().unwrap().changed, [3]);
    }

    #[test]
    fn ways_of_two_lengths_taking_turns_are_one_path() {
        let longer: &[Option<u8>] = &[Some(1), Some(2), Some(3), Some(4), Some(HOST)];
        let mut paths = Paths::default();
        record_all(
            &mut paths,
            0.0,
            &[A, longer, A, longer, longer, A, longer, longer, longer, A],
        );
        assert_eq!(
            routes(&paths),
            [vec![vec![1], vec![2], vec![3], vec![4, HOST], vec![HOST]]]
        );
    }

    #[test]
    fn silence_says_nothing_about_the_route() {
        let quiet: &[Option<u8>] = &[Some(1), None, Some(3), Some(HOST)];
        let mut paths = Paths::default();
        // A router that stops answering, then starts again.
        record_all(&mut paths, 0.0, &[A, quiet, quiet, quiet, quiet, quiet, A]);
        assert_eq!(paths.iter().count(), 1);
        assert_eq!(paths.current().unwrap().checks, 7);

        // A hop that never answered starts to.
        let mut paths = Paths::default();
        record_all(&mut paths, 0.0, &[quiet, quiet, B, B, B, B, B]);
        assert_eq!(routes(&paths), [vec![vec![1], vec![7], vec![3], vec![HOST]]]);
    }

    #[test]
    fn a_router_that_answers_too_seldom_to_tell_joins_the_path() {
        let quiet: &[Option<u8>] = &[Some(1), None, Some(3), Some(HOST)];
        let mut checks = vec![A, A, B];
        checks.extend([quiet; 11]);
        let mut paths = Paths::default();
        record_all(&mut paths, 0.0, &checks);
        assert_eq!(paths.iter().count(), 1);
        assert_eq!(paths.unsettled_since(), None, "every check is part of the path");
        assert_eq!(routes(&paths)[0][1], [2, 7]);
    }

    /// Ten checks two minutes apart from `t`, while the host is down, with a router
    /// saying it can't be reached from hop `at` on.
    fn record_down(paths: &mut Paths, t: f64, hops: &[Option<u8>], at: u8) {
        let mut down = round(hops);
        down.unreachable = Some(at);
        for i in 0..10 {
            paths.record(t + 120.0 * f64::from(i), &down);
        }
    }

    #[test]
    fn the_router_before_a_host_thats_down_ends_the_route() {
        // It says the host can't be reached for every probe that would have reached
        // the host: the route ends with it, and nothing new is at the host's hop.
        let mut paths = Paths::default();
        let t = record_all(&mut paths, 0.0, &[A, A, A]);
        record_down(&mut paths, t, &[Some(1), Some(2), Some(3), Some(3), Some(3)], 4);
        assert_eq!(routes(&paths), [vec![vec![1], vec![2], vec![3], vec![HOST]]]);
        assert_eq!(paths.unsettled_since(), None);
        let span = &paths.during(t, t + 1200.0)[0];
        assert_eq!((span.checks, span.reached), (10, 0));
        assert_eq!(span.hops[2].routers[0].checks, 10);
        assert!(span.hops[3].routers.is_empty());

        // One that doesn't answer route checks otherwise goes where it would have.
        let quiet: &[Option<u8>] = &[Some(1), Some(2), None, Some(HOST)];
        let mut paths = Paths::default();
        let t = record_all(&mut paths, 0.0, &[quiet, quiet, quiet]);
        record_down(&mut paths, t, &[Some(1), Some(2), None, Some(9), Some(9)], 4);
        assert_eq!(routes(&paths), [vec![vec![1], vec![2], vec![9], vec![HOST]]]);
        let span = &paths.during(t, t + 1200.0)[0];
        assert_eq!(span.hops[2].ms, None, "its round trip waited on the host");
    }

    #[test]
    fn a_router_that_lost_its_way_to_the_host_stays_at_its_hop() {
        let mut paths = Paths::default();
        let t = record_all(&mut paths, 0.0, &[A, A, A]);
        record_down(&mut paths, t, &[Some(1), Some(2), Some(3), Some(3), Some(3)], 3);
        assert_eq!(paths.iter().count(), 1);
        let span = &paths.during(t, t + 1200.0)[0];
        assert_eq!(span.hops[2].routers[0].checks, 10);
        assert_eq!(span.hops[2].ms, Some(30.0));
    }

    #[test]
    fn a_host_next_door_thats_down_changes_nothing() {
        // This computer itself says the host can't be reached, from the first hop.
        let next_door: &[Option<u8>] = &[Some(HOST)];
        let mut paths = Paths::default();
        let t = record_all(&mut paths, 0.0, &[next_door; 3]);
        record_down(&mut paths, t, &[Some(50), Some(50), Some(50)], 1);
        assert_eq!(routes(&paths), [vec![vec![HOST]]]);
        let span = &paths.during(t, t + 1200.0)[0];
        assert_eq!((span.checks, span.reached), (10, 0));
        assert!(span.hops.iter().all(|hop| hop.routers.is_empty()));
    }

    #[test]
    fn old_paths_and_checks_are_trimmed() {
        let mut paths = Paths::default();
        let t = record_all(&mut paths, 0.0, &[A, A, B, B, B, B, B]);
        assert_eq!(paths.iter().count(), 2);
        paths.trim(20.0);
        assert_eq!(paths.iter().count(), 2, "the first path was seen at 30");
        assert_eq!(paths.checks.len(), 6);
        paths.trim(31.0);
        assert_eq!(paths.iter().count(), 1);
        assert_eq!(paths.checks.len(), 5);
        assert_eq!(paths.during(0.0, t).len(), 1);
        // The path in use stays while checks see it, and goes once none have for that long.
        paths.trim(t - 30.0);
        assert_eq!(paths.iter().count(), 1);
        paths.trim(t);
        assert_eq!(paths.iter().count(), 0);
        assert!(paths.checks.is_empty());

        // And it starts over.
        record_all(&mut paths, t + 10.0, &[A]);
        assert_eq!(routes(&paths), [vec![vec![1], vec![2], vec![3], vec![HOST]]]);
    }

    #[test]
    fn checks_that_wait_to_be_settled_arent_trimmed() {
        let mut paths = Paths::default();
        let t = record_all(&mut paths, 0.0, &[A, B, B]);
        paths.trim(t);
        assert_eq!(paths.checks.len(), 2, "the two waiting stay");
        record_all(&mut paths, t, &[B, B]);
        assert_eq!(paths.current().unwrap().changed, [2]);
    }

    #[test]
    fn paths_are_capped() {
        let mut paths = Paths::default();
        let mut t = 0.0;
        for i in 0..MAX_PATHS + 20 {
            let route = if i % 2 == 0 { A } else { B };
            t = record_all(&mut paths, t, &[route; CONFIRM as usize]);
            // Long enough apart that none is folded back.
            t += FOLD_BACK_SECS;
        }
        assert_eq!(paths.iter().count(), MAX_PATHS);
        let first = paths.iter().next().unwrap().first;
        assert!(paths.checks.iter().all(|c| c.t >= first));
    }

    #[test]
    fn during_a_stretch_of_time() {
        let mut paths = Paths::default();
        let slow: &[Option<u8>] = &[Some(1), Some(7), Some(3), Some(HOST)];
        // Six checks on the first route, then four on the second. Round trips are 10 ms
        // a hop, but checks 3 and 4 found hop 3 and the host 50 ms slower.
        for (i, hops) in [A, A, A, A, A, A, slow, slow, slow, slow].into_iter().enumerate() {
            let mut round = round(hops);
            if i == 2 || i == 3 {
                round.routers[2].as_mut().unwrap().1 += 50.0;
                round.reached.as_mut().unwrap().1 += 50.0;
            }
            paths.record(i as f64 * 30.0, &round);
        }
        assert_eq!(paths.iter().count(), 2);

        // Around the slow checks: the first route only.
        let spans = paths.during(55.0, 95.0);
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(
            (span.checks, span.first, span.last, span.reached),
            (2, Some(60.0), Some(90.0), 2)
        );
        let ms: Vec<_> = span.hops.iter().map(|h| (h.ms, h.usual_ms)).collect();
        assert_eq!(
            ms,
            [
                (Some(10.0), Some(10.0)),
                (Some(20.0), Some(20.0)),
                (Some(80.0), Some(30.0)),
                (Some(90.0), Some(40.0)),
            ]
        );
        assert!(!span.began_within(55.0, 95.0));

        // Across the change: both, the second beginning then.
        let spans = paths.during(130.0, 220.0);
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].checks, spans[1].checks), (1, 2));
        assert!(spans[1].began_within(130.0, 220.0));
        assert_eq!(spans[1].path.changed, [2]);
        assert_eq!(spans[1].hops[1].routers, [RouterSeen { addr: ip(7), checks: 2 }]);
        assert_eq!(spans[1].hops[3].host, 2);

        // Between two checks: the path the one before saw.
        let spans = paths.during(100.0, 110.0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].checks, 0);
        assert!(spans[0].hops.iter().all(|hop| hop.ms.is_none()));

        // Long after the last check, nothing.
        assert!(paths.during(270.0 + IN_USE_SECS + 1.0, 1000.0).is_empty());
        assert!(paths.during(-100.0, -10.0).is_empty());
    }

    #[test]
    fn checks_waiting_to_be_settled_are_left_out_of_a_stretch() {
        let mut paths = Paths::default();
        let t = record_all(&mut paths, 0.0, &[A, A, B, B]);
        let spans = paths.during(0.0, t);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].checks, 2);
    }

    #[test]
    fn hops_differ_when_no_router_answered_at_both() {
        let set = |routers: &[u8], host: bool| HopSet {
            routers: routers.iter().map(|&n| ip(n)).collect(),
            host,
        };
        let a = [set(&[1], false), set(&[2, 3], false), set(&[], false), set(&[], true)];
        // Load balancing, a hop silent on one side, and the same end.
        let b = [set(&[1], false), set(&[3], false), set(&[5], false), set(&[], true)];
        assert!(differs(&a, &b).is_empty());
        let c = [
            set(&[1], false),
            set(&[4], false),
            set(&[], false),
            set(&[6], false),
            set(&[], true),
        ];
        assert_eq!(differs(&a, &c), [2, 4, 5]);
        assert_eq!(differs(&c, &a), [2, 4]);
        // Past an end the two share, a longer way isn't a difference.
        let d = [
            set(&[1], false),
            set(&[2], false),
            set(&[], false),
            set(&[6], true),
            set(&[], true),
        ];
        assert!(differs(&a, &d).is_empty());
        // Nor is anything past where a path that never reached the host stopped.
        let e = [set(&[1], false), set(&[2], false)];
        assert!(differs(&e, &a).is_empty());
    }

    #[test]
    fn round_trips_are_kept_to_a_tenth_of_a_millisecond() {
        assert_eq!(Rtt::new(12.34).ms(), Some(12.3));
        assert_eq!(Rtt::new(0.0).ms(), Some(0.0));
        assert_eq!(Rtt::new(-1.0).ms(), Some(0.0));
        assert_eq!(Rtt::new(1e9).ms(), Some(6553.4));
        assert_eq!(Rtt::NONE.ms(), None);
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&mut []), None);
    }

    #[test]
    fn clearing_forgets_everything() {
        let mut paths = Paths::default();
        record_all(&mut paths, 0.0, &[A, A, B, B]);
        let version = paths.version();
        paths.clear();
        assert_eq!(paths.iter().count(), 0);
        assert_eq!(paths.unsettled_since(), None);
        assert!(paths.version() > version);
        record_all(&mut paths, 200.0, &[B]);
        assert_eq!(routes(&paths), [vec![vec![1], vec![7], vec![3], vec![HOST]]]);
    }
}
