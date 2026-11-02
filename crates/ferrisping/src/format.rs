//! Number, duration and time-of-day formatting for the UI.

use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;

/// A latency value without unit: one decimal below 10 ms, whole numbers above.
pub fn ms(value: f32) -> String {
    if value < 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}

pub fn ms_unit(value: f32) -> String {
    format!("{} ms", ms(value))
}

/// `0%`, `0.3%`, `12%`, `100%`.
pub fn percent(value: f32) -> String {
    if value == 0.0 {
        "0%".into()
    } else if value < 10.0 {
        format!("{value:.1}%")
    } else {
        format!("{value:.0}%")
    }
}

/// `45s`, `7m 36s`, `2h 05m`, `3d 4h`.
pub fn duration(secs: f64) -> String {
    let secs = secs.max(0.0).round() as u64;
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// `0.25 s`, `1 s`, `1 min`.
pub fn interval(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 60.0 && secs % 60.0 == 0.0 {
        format!("{} min", secs / 60.0)
    } else {
        format!("{secs} s")
    }
}

/// Formats sample times in the computer's local time zone.
#[derive(Clone)]
pub struct Clock {
    tz: TimeZone,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            tz: TimeZone::try_system().unwrap_or(TimeZone::UTC),
        }
    }

    fn zoned(&self, t: f64) -> jiff::Zoned {
        let ts = Timestamp::from_microsecond((t * 1e6) as i64).unwrap_or(Timestamp::UNIX_EPOCH);
        ts.to_zoned(self.tz.clone())
    }

    /// Seconds to add to a UNIX time to get local wall-clock seconds (for aligning grid lines).
    pub fn utc_offset_secs(&self, t: f64) -> f64 {
        self.zoned(t).offset().seconds() as f64
    }

    /// `8:38:30 PM`
    pub fn time(&self, t: f64) -> String {
        trim_hour(self.zoned(t).strftime("%I:%M:%S %p").to_string())
    }

    /// `8:38 PM`
    pub fn time_short(&self, t: f64) -> String {
        trim_hour(self.zoned(t).strftime("%I:%M %p").to_string())
    }

    /// `Mon 31`, for day-sized axis steps.
    pub fn day(&self, t: f64) -> String {
        let zoned = self.zoned(t);
        format!("{} {}", zoned.strftime("%a"), zoned.day())
    }

    /// `Mon 8:38 PM`, for events older than today.
    pub fn day_time(&self, t: f64) -> String {
        let zoned = self.zoned(t);
        let today = self.zoned(ferrisping_core::now_secs()).date();
        if zoned.date() == today {
            self.time(t)
        } else {
            format!("{} {}", zoned.strftime("%a"), self.time_short(t))
        }
    }
}

fn trim_hour(text: String) -> String {
    text.strip_prefix('0').map(str::to_string).unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(5.4), "5s");
        assert_eq!(duration(456.0), "7m 36s");
        assert_eq!(duration(7500.0), "2h 05m");
        assert_eq!(duration(3.0 * 86_400.0 + 4.0 * 3600.0), "3d 4h");
    }

    #[test]
    fn latency_precision() {
        assert_eq!(ms(0.42), "0.4");
        assert_eq!(ms(46.43), "46");
    }

    #[test]
    fn labels() {
        assert_eq!(interval(Duration::from_millis(250)), "0.25 s");
        assert_eq!(interval(Duration::from_secs(60)), "1 min");
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(0.33), "0.3%");
        assert_eq!(percent(100.0), "100%");
    }
}
