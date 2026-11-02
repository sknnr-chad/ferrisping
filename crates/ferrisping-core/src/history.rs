//! Per-host sample history and the statistics shown in the UI.

use std::collections::VecDeque;

use crate::icmp::EchoStatus;

/// What happened to one probe.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    /// Sent, still waiting for the answer or the timeout.
    Pending,
    Reply {
        rtt_ms: f32,
    },
    Lost(EchoStatus),
}

/// One probe, stamped with the time it was sent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Send time, in seconds since the UNIX epoch (see [`crate::now_secs`]).
    pub t: f64,
    pub outcome: Outcome,
}

impl Sample {
    pub fn rtt_ms(&self) -> Option<f32> {
        match self.outcome {
            Outcome::Reply { rtt_ms } => Some(rtt_ms),
            _ => None,
        }
    }

    pub fn is_lost(&self) -> bool {
        matches!(self.outcome, Outcome::Lost(_))
    }
}

/// Samples in send order, addressed by a sequence number that keeps counting
/// as old samples are trimmed.
#[derive(Clone, Debug, Default)]
pub struct History {
    samples: VecDeque<Sample>,
    first_seq: u64,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a probe that was just sent; returns its sequence number.
    pub fn push_pending(&mut self, t: f64) -> u64 {
        self.samples.push_back(Sample {
            t,
            outcome: Outcome::Pending,
        });
        self.next_seq() - 1
    }

    /// Fill in a probe's outcome. Returns false if the sample was already trimmed or cleared.
    pub fn complete(&mut self, seq: u64, outcome: Outcome) -> bool {
        match self.index(seq).and_then(|i| self.samples.get_mut(i)) {
            Some(sample) => {
                sample.outcome = outcome;
                true
            }
            None => false,
        }
    }

    pub fn get(&self, seq: u64) -> Option<&Sample> {
        self.index(seq).and_then(|i| self.samples.get(i))
    }

    fn index(&self, seq: u64) -> Option<usize> {
        seq.checked_sub(self.first_seq).map(|i| i as usize)
    }

    /// The sequence number the next pushed sample will get.
    pub fn next_seq(&self) -> u64 {
        self.first_seq + self.samples.len() as u64
    }

    pub fn first_seq(&self) -> u64 {
        self.first_seq
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Drop samples sent before `t`, or beyond `max_len`, but never at or after `keep_from_seq`.
    pub fn trim(&mut self, t: f64, max_len: usize, keep_from_seq: u64) {
        while let Some(front) = self.samples.front() {
            let too_old = front.t < t || self.samples.len() > max_len;
            if !too_old || self.first_seq >= keep_from_seq {
                break;
            }
            self.samples.pop_front();
            self.first_seq += 1;
        }
    }

    /// Forget everything; probes still in flight are ignored when they finish.
    pub fn clear(&mut self) {
        self.first_seq = self.next_seq();
        self.samples.clear();
    }

    /// Samples sent in `[t0, t1]`, oldest first.
    pub fn range(&self, t0: f64, t1: f64) -> impl Iterator<Item = &Sample> {
        let start = self.samples.partition_point(|s| s.t < t0);
        let end = self.samples.partition_point(|s| s.t <= t1).max(start);
        self.samples.range(start..end)
    }

    /// The most recent finished sample, if any.
    pub fn last_finished(&self) -> Option<&Sample> {
        self.samples.iter().rev().find(|s| s.outcome != Outcome::Pending)
    }

    /// Statistics over the samples sent in `[t0, t1]`. Pending probes don't count.
    pub fn stats(&self, t0: f64, t1: f64) -> WindowStats {
        let mut stats = WindowStats::default();
        let mut sum = 0.0f64;
        let mut jitter_sum = 0.0f64;
        let mut jitter_n = 0u32;
        let mut prev: Option<f32> = None;
        for sample in self.range(t0, t1) {
            match sample.outcome {
                Outcome::Pending => {}
                Outcome::Lost(_) => {
                    stats.sent += 1;
                    stats.lost += 1;
                }
                Outcome::Reply { rtt_ms } => {
                    stats.sent += 1;
                    stats.received += 1;
                    sum += rtt_ms as f64;
                    stats.min_ms = Some(stats.min_ms.map_or(rtt_ms, |m| m.min(rtt_ms)));
                    stats.max_ms = Some(stats.max_ms.map_or(rtt_ms, |m| m.max(rtt_ms)));
                    if let Some(p) = prev {
                        jitter_sum += (rtt_ms - p).abs() as f64;
                        jitter_n += 1;
                    }
                    prev = Some(rtt_ms);
                }
            }
        }
        if stats.received > 0 {
            stats.avg_ms = Some((sum / stats.received as f64) as f32);
        }
        if jitter_n > 0 {
            stats.jitter_ms = Some((jitter_sum / jitter_n as f64) as f32);
        }
        stats
    }
}

/// Summary of a time window for one host.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WindowStats {
    /// Probes with a final outcome.
    pub sent: u32,
    pub received: u32,
    pub lost: u32,
    pub min_ms: Option<f32>,
    pub avg_ms: Option<f32>,
    pub max_ms: Option<f32>,
    /// Mean change between consecutive replies.
    pub jitter_ms: Option<f32>,
}

impl WindowStats {
    pub fn loss_pct(&self) -> Option<f32> {
        (self.sent > 0).then(|| self.lost as f32 * 100.0 / self.sent as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(ms: f32) -> Outcome {
        Outcome::Reply { rtt_ms: ms }
    }

    #[test]
    fn sequence_numbers_survive_trimming() {
        let mut h = History::new();
        for i in 0..5 {
            h.push_pending(i as f64);
        }
        h.trim(2.0, usize::MAX, u64::MAX);
        assert_eq!((h.first_seq(), h.len()), (2, 3));
        assert!(!h.complete(1, reply(1.0)), "trimmed sample can't complete");
        assert!(h.complete(3, reply(1.0)));
        assert_eq!(h.get(3).unwrap().rtt_ms(), Some(1.0));
    }

    #[test]
    fn trim_keeps_unprocessed_samples() {
        let mut h = History::new();
        for i in 0..5 {
            h.push_pending(i as f64);
        }
        h.trim(10.0, usize::MAX, 1);
        assert_eq!(h.first_seq(), 1);
    }

    #[test]
    fn stats_ignore_pending_and_count_loss() {
        let mut h = History::new();
        for (i, outcome) in [
            reply(10.0),
            Outcome::Lost(EchoStatus::TimedOut),
            reply(20.0),
            reply(14.0),
        ]
        .into_iter()
        .enumerate()
        {
            let seq = h.push_pending(i as f64);
            h.complete(seq, outcome);
        }
        h.push_pending(4.0);
        let s = h.stats(0.0, 10.0);
        assert_eq!((s.sent, s.received, s.lost), (4, 3, 1));
        assert_eq!((s.min_ms, s.max_ms), (Some(10.0), Some(20.0)));
        assert!((s.avg_ms.unwrap() - 14.666_667).abs() < 1e-3);
        assert_eq!(s.jitter_ms, Some(8.0));
        assert_eq!(s.loss_pct(), Some(25.0));
    }

    #[test]
    fn range_is_inclusive_and_ordered() {
        let mut h = History::new();
        for i in 0..10 {
            h.push_pending(i as f64);
        }
        let ts: Vec<f64> = h.range(2.0, 4.0).map(|s| s.t).collect();
        assert_eq!(ts, [2.0, 3.0, 4.0]);
        assert_eq!(h.range(20.0, 30.0).count(), 0);
    }

    #[test]
    fn clear_drops_in_flight_probes() {
        let mut h = History::new();
        let seq = h.push_pending(0.0);
        h.clear();
        assert!(!h.complete(seq, reply(1.0)));
        assert_eq!(h.push_pending(1.0), seq + 1);
    }
}
