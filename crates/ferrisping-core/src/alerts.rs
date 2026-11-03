//! Alerts: which events are worth telling the user about, and when a host's loss
//! or round trips cross the limits set for them.
//!
//! Loss and round trips are judged over a host's last few pings at each reply,
//! right after the up/down logic, so alerts work whether or not anything is drawn.
//! A host that stops answering raises an outage alert, not a loss alert, and its
//! outage doesn't count toward its loss once it's back. Crossing a limit is logged
//! as an event, like an outage, and the hook set with
//! [`crate::Monitor::set_on_alert`] hears about every alert as it's logged.

use crate::engine::{Event, EventKind};
use crate::history::{History, Outcome};

/// Limits on a host's loss and round trips over its last few pings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// How many of the latest pings are judged.
    pub pings: u32,
    /// Alert when at least this many percent of them get no reply.
    pub loss_pct: Option<f32>,
    /// Alert when their replies average more than this many milliseconds.
    pub slow_ms: Option<f32>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pings: 30,
            loss_pct: None,
            slow_ms: None,
        }
    }
}

/// Which events are alerts, for every host without alerts of its own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AlertSettings {
    /// A host stopping answering, and answering again.
    pub outages: bool,
    /// Traffic to a host switching to another network.
    pub switches: bool,
    pub limits: Limits,
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self {
            outages: true,
            switches: true,
            limits: Limits::default(),
        }
    }
}

/// A host's own alerts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum HostAlerts {
    /// The same as other hosts.
    #[default]
    Usual,
    /// None at all.
    Off,
    /// Limits of its own. Its outages and switches are alerts as for other hosts.
    Own(Limits),
}

impl HostAlerts {
    /// The limits a host is judged by, given everyone else's.
    pub fn limits(self, usual: Limits) -> Option<Limits> {
        match self {
            Self::Usual => Some(usual),
            Self::Off => None,
            Self::Own(limits) => Some(limits),
        }
    }
}

/// An event that's an alert for its host, with what a notification needs to say.
#[derive(Clone, Debug)]
pub struct Alert {
    pub event: Event,
    /// For a switch to another network: what the networks the traffic left
    /// through and now leaves through are called.
    pub networks: Option<(String, String)>,
}

/// Where a host stands with its limits.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Watch {
    /// The first ping that counts. Earlier ones came before an outage, or before
    /// the history was cleared.
    from_seq: u64,
    loss: Level,
    slow: Level,
}

/// One limit, crossed or not.
#[derive(Clone, Copy, Debug, Default)]
struct Level {
    /// Crossed, and alerted.
    over: bool,
    /// Pings in a row judged within the limit since.
    within_for: u32,
}

impl Level {
    /// Take one judgment. Returns `Some(true)` when the limit is crossed, and
    /// `Some(false)` once things have been within it for `pings` pings in a row, so
    /// a host that sits right on the limit isn't reported again and again.
    fn judge(&mut self, over: bool, pings: u32) -> Option<bool> {
        if !self.over {
            self.over = over;
            return over.then_some(true);
        }
        if over {
            self.within_for = 0;
            return None;
        }
        self.within_for += 1;
        if self.within_for < pings {
            return None;
        }
        *self = Level::default();
        Some(false)
    }
}

impl Watch {
    /// Start over: only pings from `from_seq` on count, and no limit is crossed.
    pub(crate) fn restart(&mut self, from_seq: u64) {
        *self = Watch {
            from_seq,
            ..Watch::default()
        };
    }

    /// Forget which limits were crossed, without an event, but keep the pings
    /// that count: for when the limits change.
    pub(crate) fn reset(&mut self) {
        self.loss = Level::default();
        self.slow = Level::default();
    }

    /// Judge the last `limits.pings` pings up to `seq`, the one that just got its
    /// result. Every ping up to it has one, since results are taken in send order.
    pub(crate) fn judge(&mut self, history: &History, seq: u64, limits: Limits) -> Vec<EventKind> {
        let mut kinds = Vec::new();
        let pings = limits.pings.max(1);
        // Not enough pings yet since the host was added or came back.
        let Some(first) = (seq + 1)
            .checked_sub(u64::from(pings))
            .filter(|&first| first >= self.from_seq)
        else {
            return kinds;
        };
        let (mut lost, mut replies, mut total_ms) = (0u32, 0u32, 0.0f64);
        for sample in first..=seq {
            match history.get(sample).map(|s| s.outcome) {
                Some(Outcome::Reply { rtt_ms }) => {
                    replies += 1;
                    total_ms += f64::from(rtt_ms);
                }
                Some(Outcome::Lost(_)) => lost += 1,
                // Trimmed away or cleared: too little left to judge.
                Some(Outcome::Pending) | None => return kinds,
            }
        }
        if let Some(limit_pct) = limits.loss_pct {
            let over = lost as f32 * 100.0 / pings as f32 >= limit_pct;
            match self.loss.judge(over, pings) {
                Some(true) => kinds.push(EventKind::Loss { lost, pings, limit_pct }),
                Some(false) => kinds.push(EventKind::LossEnded { limit_pct, pings }),
                None => {}
            }
        }
        // Without a single reply there are no round trips to judge, and the host is
        // about to count as down anyway.
        if let Some(limit_ms) = limits.slow_ms
            && replies > 0
        {
            let avg_ms = (total_ms / f64::from(replies)) as f32;
            match self.slow.judge(avg_ms > limit_ms, pings) {
                Some(true) => kinds.push(EventKind::Slow {
                    avg_ms,
                    pings,
                    limit_ms,
                }),
                Some(false) => kinds.push(EventKind::SlowEnded { limit_ms, pings }),
                None => {}
            }
        }
        kinds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icmp::EchoStatus;

    const LOST: Outcome = Outcome::Lost(EchoStatus::TimedOut);

    fn reply(rtt_ms: f32) -> Outcome {
        Outcome::Reply { rtt_ms }
    }

    /// Feeds outcomes through a history one at a time, as the engine does, and
    /// collects the events, numbered by the ping that brought each one.
    struct Feed {
        history: History,
        watch: Watch,
        limits: Limits,
    }

    impl Feed {
        fn new(limits: Limits) -> Self {
            Self {
                history: History::new(),
                watch: Watch::default(),
                limits,
            }
        }

        fn push(&mut self, outcomes: impl IntoIterator<Item = Outcome>) -> Vec<(u64, EventKind)> {
            let mut events = Vec::new();
            for outcome in outcomes {
                let seq = self.history.push_pending(self.history.next_seq() as f64);
                self.history.complete(seq, outcome);
                events.extend(
                    self.watch
                        .judge(&self.history, seq, self.limits)
                        .into_iter()
                        .map(|kind| (seq, kind)),
                );
            }
            events
        }
    }

    fn loss(pct: f32) -> Limits {
        Limits {
            pings: 10,
            loss_pct: Some(pct),
            slow_ms: None,
        }
    }

    #[test]
    fn loss_is_judged_once_there_are_enough_pings() {
        let mut feed = Feed::new(loss(20.0));
        // Two of the first two lost is 100%, but that's too few pings to judge.
        assert!(feed.push([LOST, LOST]).is_empty());
        let events = feed.push(std::iter::repeat_n(reply(5.0), 7));
        assert!(events.is_empty(), "{events:?}");
        // The tenth ping makes a full window: 2 of 10 lost is 20%.
        assert_eq!(
            feed.push([reply(5.0)]),
            [(
                9,
                EventKind::Loss {
                    lost: 2,
                    pings: 10,
                    limit_pct: 20.0
                }
            )]
        );
    }

    #[test]
    fn crossing_the_limit_is_reported_once_and_its_end_after_a_full_window() {
        let mut feed = Feed::new(loss(20.0));
        feed.push(std::iter::repeat_n(reply(5.0), 10));
        let events = feed.push([LOST, LOST]);
        assert_eq!(events.len(), 1, "{events:?}");
        // Sitting on the limit: more loss now and then doesn't report it again.
        let mut more = Vec::new();
        for _ in 0..3 {
            more.extend(feed.push([reply(5.0), reply(5.0), reply(5.0), LOST]));
        }
        assert!(more.is_empty(), "{more:?}");
        // Ping 29 leaves one loss in the window, which is within the limit, and it
        // has to stay that way for 10 pings in a row.
        let events = feed.push(std::iter::repeat_n(reply(5.0), 30));
        assert_eq!(
            events,
            [(
                38,
                EventKind::LossEnded {
                    limit_pct: 20.0,
                    pings: 10
                }
            )]
        );
    }

    #[test]
    fn round_trips_over_the_limit_are_slow() {
        let limits = Limits {
            pings: 4,
            loss_pct: None,
            slow_ms: Some(100.0),
        };
        let mut feed = Feed::new(limits);
        // An average of 92.5 ms.
        assert!(
            feed.push([reply(10.0), reply(30.0), reply(270.0), reply(60.0)])
                .is_empty()
        );
        // A lost ping doesn't count toward the average, so it's 120 ms now, not 90.
        assert_eq!(
            feed.push([LOST]),
            [(
                4,
                EventKind::Slow {
                    avg_ms: 120.0,
                    pings: 4,
                    limit_ms: 100.0
                }
            )]
        );
        // Within the limit from ping 6, and then for 4 pings in a row.
        let events = feed.push(std::iter::repeat_n(reply(20.0), 8));
        assert_eq!(
            events,
            [(
                9,
                EventKind::SlowEnded {
                    limit_ms: 100.0,
                    pings: 4
                }
            )]
        );
    }

    #[test]
    fn a_restart_counts_only_later_pings() {
        let mut feed = Feed::new(loss(50.0));
        feed.push(std::iter::repeat_n(LOST, 10));
        // Back after an outage: the lost pings before it don't count.
        feed.watch.restart(feed.history.next_seq());
        assert!(feed.push(std::iter::repeat_n(reply(5.0), 12)).is_empty());
    }

    #[test]
    fn a_reset_forgets_a_crossed_limit_without_an_event() {
        let mut feed = Feed::new(loss(20.0));
        assert_eq!(feed.push(std::iter::repeat_n(LOST, 10)).len(), 1);
        assert!(
            feed.push(std::iter::repeat_n(reply(5.0), 10)).is_empty(),
            "not over yet"
        );
        feed.watch.reset();
        assert!(
            feed.push(std::iter::repeat_n(reply(5.0), 20)).is_empty(),
            "no end without a start"
        );
        // Judged afresh, a window over the limit crosses it at once.
        assert_eq!(feed.push([LOST, LOST]).len(), 1);
    }

    #[test]
    fn host_alerts_pick_their_limits() {
        let usual = loss(10.0);
        assert_eq!(HostAlerts::Usual.limits(usual), Some(usual));
        assert_eq!(HostAlerts::Off.limits(usual), None);
        assert_eq!(HostAlerts::Own(loss(50.0)).limits(usual), Some(loss(50.0)));
    }
}
