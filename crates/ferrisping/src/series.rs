//! Turns a host's history into what a lane draws: decimated line runs,
//! packet-loss bands, the latency scale and the time grid.

use egui_plot::GridMark;
use ferrisping_core::{History, Outcome, Sample};

/// Time grid steps in seconds. Each divides the next, so minor lines always
/// line up with the labeled ones.
const TIME_STEPS: [f64; 13] = [
    1.0, 5.0, 15.0, 30.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 10_800.0, 21_600.0, 43_200.0, 86_400.0,
];
/// Minimum distance between time labels, in screen points.
const LABEL_SPACING: f64 = 110.0;
/// Minimum distance between minor grid lines, in screen points.
const MINOR_SPACING: f64 = 24.0;
/// Never return more marks than this, whatever the zoom.
const MAX_MARKS: i64 = 2_000;

fn step_at_least(secs: f64) -> f64 {
    TIME_STEPS
        .iter()
        .copied()
        .find(|step| *step >= secs)
        .unwrap_or_else(|| 86_400.0 * (secs / 86_400.0).ceil())
}

/// The step between labeled time marks for a given zoom.
pub fn label_step(secs_per_point: f64) -> f64 {
    step_at_least(LABEL_SPACING * secs_per_point)
}

/// Multiples of `step` in local wall-clock time that fall inside `[lo, hi]`, each
/// with the UTC offset it's aligned to. `utc_offset` gives the offset at a moment:
/// when it changes inside the range (daylight saving starting or ending), the
/// marks on each side line up with that side's local time.
pub fn aligned(lo: f64, hi: f64, step: f64, utc_offset: impl Fn(f64) -> f64) -> Vec<(f64, f64)> {
    let marks = |offset: f64| {
        let first = ((lo + offset) / step).ceil() as i64;
        let last = ((hi + offset) / step).floor() as i64;
        let last = last.min(first + MAX_MARKS);
        (first..=last).map(move |k| (k as f64 * step - offset, offset))
    };
    let (start, end) = (utc_offset(lo), utc_offset(hi));
    if start == end {
        return marks(start).collect();
    }
    let mut both: Vec<(f64, f64)> = marks(start)
        .chain(marks(end))
        .filter(|&(t, offset)| utc_offset(t) == offset)
        .collect();
    both.sort_by(|a, b| a.0.total_cmp(&b.0));
    both
}

/// Grid marks for the time axis: faint minor lines plus stronger lines where labels go.
#[expect(clippy::neg_cmp_op_on_partial_ord, reason = "the negated tests also reject NaN")]
pub fn time_marks(lo: f64, hi: f64, secs_per_point: f64, utc_offset: impl Fn(f64) -> f64) -> Vec<GridMark> {
    if !(hi > lo) || !(secs_per_point > 0.0) {
        return Vec::new();
    }
    let major = label_step(secs_per_point);
    let minor = step_at_least(MINOR_SPACING * secs_per_point).min(major);
    aligned(lo, hi, minor, utc_offset)
        .into_iter()
        .map(|(value, offset)| {
            let local = value + offset;
            let on_major = ((local / major).round() * major - local).abs() < 1e-6 * major;
            GridMark {
                value,
                step_size: if on_major { major } else { minor },
            }
        })
        .collect()
}

/// A rounded top for a latency axis with some headroom above `max_ms`.
pub fn latency_top(max_ms: f32) -> f64 {
    const NICE: [f64; 10] = [1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0];
    let target = (max_ms as f64 * 1.15).max(1.0);
    let magnitude = 10f64.powf(target.log10().floor());
    NICE.iter()
        .map(|n| n * magnitude)
        .find(|v| *v >= target * (1.0 - 1e-9))
        .unwrap_or(10.0 * magnitude)
}

/// Latency grid lines from zero to the top in `steps` even steps: two steps (zero,
/// half way and the top) for a full graph, one for a short one.
#[expect(clippy::neg_cmp_op_on_partial_ord, reason = "the negated tests also reject NaN")]
pub fn latency_marks(top: f64, steps: u32) -> Vec<GridMark> {
    if !(top > 0.0) || steps == 0 {
        return Vec::new();
    }
    let step = top / steps as f64;
    (0..=steps)
        .map(|i| GridMark {
            value: step * i as f64,
            step_size: step,
        })
        .collect()
}

/// `0`, `0.75`, `2.5`, `30`, `150`.
pub fn latency_tick(value: f64) -> String {
    if value >= 10.0 || value == 0.0 {
        format!("{value:.0}")
    } else {
        let text = format!("{value:.2}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// What one lane draws for a time range.
#[derive(Debug, Default, PartialEq)]
pub struct Series {
    /// Unbroken stretches of replies as `[time, ms]`, at most two points per bucket.
    pub runs: Vec<Vec<[f64; 2]>>,
    /// Time ranges where probes were lost.
    pub losses: Vec<(f64, f64)>,
    /// Highest round trip inside the range.
    pub max_ms: f32,
}

/// The lowest and highest reply inside one screen-point-wide bucket.
struct Bucket {
    index: i64,
    low: [f64; 2],
    high: [f64; 2],
}

impl Bucket {
    fn new(index: i64, point: [f64; 2]) -> Self {
        Self {
            index,
            low: point,
            high: point,
        }
    }

    fn add(&mut self, point: [f64; 2]) {
        if point[1] < self.low[1] {
            self.low = point;
        }
        if point[1] > self.high[1] {
            self.high = point;
        }
    }

    fn emit(self, run: &mut Vec<[f64; 2]>) {
        let (first, second) = if self.low[0] <= self.high[0] {
            (self.low, self.high)
        } else {
            (self.high, self.low)
        };
        run.push(first);
        if second != first {
            run.push(second);
        }
    }
}

/// Build the drawable series for `[t0, t1]`, decimated to `buckets` columns.
///
/// `interval` is the current probe interval, used to size the band for a trailing
/// loss and to break lines across long gaps (pauses, sleep).
pub fn build(history: &History, t0: f64, t1: f64, buckets: usize, interval: f64) -> Series {
    let bucket_secs = ((t1 - t0) / buckets.max(1) as f64).max(1e-9);
    let gap_limit = (interval * 5.0).max(10.0);
    let mut series = Series::default();
    let mut run: Vec<[f64; 2]> = Vec::new();
    let mut bucket: Option<Bucket> = None;
    let mut last_t: Option<f64> = None;
    let mut loss_start: Option<f64> = None;
    let mut last_loss = 0.0;

    // Reach one probe past each edge so lines continue off the visible range.
    let margin = interval.max(bucket_secs);
    for sample in history.range(t0 - margin, t1 + margin) {
        match sample.outcome {
            Outcome::Pending => continue,
            Outcome::Lost(_) => {
                end_run(&mut run, &mut bucket, &mut series.runs);
                loss_start.get_or_insert(sample.t);
                last_loss = sample.t;
            }
            Outcome::Reply { rtt_ms } => {
                if let Some(start) = loss_start.take() {
                    series.losses.push((start, sample.t));
                }
                if last_t.is_some_and(|prev| sample.t - prev > gap_limit) {
                    end_run(&mut run, &mut bucket, &mut series.runs);
                }
                if (t0..=t1).contains(&sample.t) {
                    series.max_ms = series.max_ms.max(rtt_ms);
                }
                let point = [sample.t, rtt_ms as f64];
                let index = ((sample.t - t0) / bucket_secs).floor() as i64;
                match &mut bucket {
                    Some(current) if current.index == index => current.add(point),
                    _ => {
                        if let Some(done) = bucket.take() {
                            done.emit(&mut run);
                        }
                        bucket = Some(Bucket::new(index, point));
                    }
                }
            }
        }
        last_t = Some(sample.t);
    }
    end_run(&mut run, &mut bucket, &mut series.runs);
    if let Some(start) = loss_start {
        series.losses.push((start, last_loss + interval));
    }
    series.losses = merge_losses(series.losses, bucket_secs * 1.5);
    series
}

fn end_run(run: &mut Vec<[f64; 2]>, bucket: &mut Option<Bucket>, runs: &mut Vec<Vec<[f64; 2]>>) {
    if let Some(b) = bucket.take() {
        b.emit(run);
    }
    if !run.is_empty() {
        runs.push(std::mem::take(run));
    }
}

/// Join loss bands closer than `min_width`, and widen each to at least `min_width`
/// so a single lost probe stays visible when zoomed out.
fn merge_losses(losses: Vec<(f64, f64)>, min_width: f64) -> Vec<(f64, f64)> {
    let mut merged: Vec<(f64, f64)> = Vec::with_capacity(losses.len());
    for (start, end) in losses {
        let end = end.max(start + min_width);
        match merged.last_mut() {
            Some(last) if start - last.1 < min_width => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// The finished sample closest to `t`, if one lies within `max_dt` seconds.
pub fn nearest(history: &History, t: f64, max_dt: f64) -> Option<Sample> {
    history
        .range(t - max_dt, t + max_dt)
        .filter(|s| s.outcome != Outcome::Pending)
        .min_by(|a, b| (a.t - t).abs().total_cmp(&(b.t - t).abs()))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrisping_core::EchoStatus;

    fn history(samples: &[(f64, Option<f32>)]) -> History {
        let mut h = History::new();
        for &(t, rtt) in samples {
            let seq = h.push_pending(t);
            let outcome = match rtt {
                Some(rtt_ms) => Outcome::Reply { rtt_ms },
                None => Outcome::Lost(EchoStatus::TimedOut),
            };
            h.complete(seq, outcome);
        }
        h
    }

    #[test]
    fn losses_break_lines_and_become_bands() {
        let h = history(&[
            (1.0, Some(10.0)),
            (2.0, Some(12.0)),
            (3.0, None),
            (4.0, None),
            (5.0, Some(11.0)),
            (6.0, Some(13.0)),
        ]);
        let s = build(&h, 0.0, 10.0, 1000, 1.0);
        assert_eq!(s.runs.len(), 2);
        assert_eq!(s.runs[0], vec![[1.0, 10.0], [2.0, 12.0]]);
        assert_eq!(s.losses, vec![(3.0, 5.0)]);
        assert_eq!(s.max_ms, 13.0);
    }

    #[test]
    fn trailing_loss_extends_one_interval() {
        let h = history(&[(1.0, Some(10.0)), (2.0, None)]);
        let s = build(&h, 0.0, 10.0, 1000, 1.0);
        assert_eq!(s.losses, vec![(2.0, 3.0)]);
    }

    #[test]
    fn dense_samples_keep_min_and_max_per_bucket() {
        let samples: Vec<_> = (0..100).map(|i| (i as f64 * 0.01, Some((i % 7) as f32))).collect();
        let s = build(&history(&samples), 0.0, 1.0, 2, 0.01);
        let points: usize = s.runs.iter().map(Vec::len).sum();
        assert!(points <= 4, "{points} points");
        assert_eq!(s.max_ms, 6.0);
    }

    #[test]
    fn long_gaps_break_lines() {
        let h = history(&[
            (1.0, Some(10.0)),
            (2.0, Some(10.0)),
            (100.0, Some(10.0)),
            (101.0, Some(10.0)),
        ]);
        let s = build(&h, 0.0, 200.0, 1000, 1.0);
        assert_eq!(s.runs.len(), 2);
    }

    #[test]
    fn single_losses_stay_visible_when_zoomed_out() {
        let h = history(&[(1.0, Some(10.0)), (2.0, None), (2.001, Some(10.0))]);
        let s = build(&h, 0.0, 1000.0, 100, 1.0);
        assert_eq!(s.losses.len(), 1);
        assert!(s.losses[0].1 - s.losses[0].0 >= 15.0);
    }

    #[test]
    fn latency_axis_tops_are_round() {
        assert_eq!(latency_top(46.0), 60.0);
        assert_eq!(latency_top(4.0), 5.0);
        assert_eq!(latency_top(0.2), 1.0);
        assert_eq!(latency_top(850.0), 1000.0);
        assert_eq!(latency_top(870.0), 1500.0);
        assert_eq!(latency_tick(0.75), "0.75");
        assert_eq!(latency_tick(2.5), "2.5");
        assert_eq!(latency_tick(30.0), "30");
    }

    #[test]
    fn latency_grid_lines() {
        let values = |steps| latency_marks(60.0, steps).iter().map(|m| m.value).collect::<Vec<_>>();
        assert_eq!(values(2), [0.0, 30.0, 60.0]);
        assert_eq!(values(1), [0.0, 60.0]);
        assert!(latency_marks(0.0, 2).is_empty());
        assert!(latency_marks(f64::NAN, 2).is_empty());
    }

    #[test]
    fn time_marks_align_to_local_time() {
        // 0.5 s per point: labels every 60 s, minor lines every 15 s.
        let offset = -7.0 * 3600.0;
        let marks = time_marks(1000.0, 1300.0, 0.5, |_| offset);
        let majors: Vec<f64> = marks.iter().filter(|m| m.step_size == 60.0).map(|m| m.value).collect();
        assert_eq!(majors, [1020.0, 1080.0, 1140.0, 1200.0, 1260.0]);
        assert!(marks.iter().all(|m| ((m.value + offset) % 15.0).abs() < 1e-9));
    }

    #[test]
    fn time_marks_follow_a_daylight_saving_change() {
        // Mountain time fell back from UTC-6 to UTC-7 at 2 AM on 1 November 2026.
        let change = 1_793_520_000.0;
        let offset = |t: f64| if t < change { -6.0 * 3600.0 } else { -7.0 * 3600.0 };
        // A day around it, labeled every 3 hours: 3 PM to midnight in daylight
        // time, then 3 AM to noon in standard time.
        let (lo, hi) = (change - 12.0 * 3600.0, change + 12.0 * 3600.0);
        let marks = time_marks(lo, hi, 60.0, offset);
        let majors: Vec<f64> = marks
            .iter()
            .filter(|m| m.step_size == 10_800.0)
            .map(|m| m.value)
            .collect();
        let hours: Vec<f64> = majors.iter().map(|t| (t - change) / 3600.0).collect();
        assert_eq!(hours, [-11.0, -8.0, -5.0, -2.0, 2.0, 5.0, 8.0, 11.0]);
        // The labels go on the same lines.
        let labels: Vec<f64> = aligned(lo, hi, 10_800.0, offset).into_iter().map(|(t, _)| t).collect();
        assert_eq!(labels, majors);
    }

    #[test]
    fn nearest_skips_pending() {
        let mut h = history(&[(1.0, Some(10.0))]);
        h.push_pending(1.4);
        assert_eq!(nearest(&h, 1.3, 1.0).map(|s| s.t), Some(1.0));
        assert_eq!(nearest(&h, 5.0, 1.0), None);
    }
}
