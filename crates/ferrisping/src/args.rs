//! Command line: `ferrisping [hosts...] [--interval 0.5] [--window 15m] [--theme dark]`.
//!
//! Hosts given on the command line make a one-off session that isn't saved.
//! `--route <host>` watches that host and opens its traceroute.
//! `--screenshot <png> [--screenshot-after <secs>] [--exit-after-screenshot]`
//! saves an image of the window, for docs and visual checks.

use std::path::PathBuf;
use std::time::Duration;

use ferrisping_core::{TargetSpec, parse_targets};

use crate::app::MIN_INTERVAL;
use crate::theme::ThemeChoice;

#[derive(Debug, Default)]
pub struct Args {
    pub hosts: Vec<TargetSpec>,
    pub interval: Option<Duration>,
    pub window_secs: Option<u64>,
    pub theme: Option<ThemeChoice>,
    pub route: Option<TargetSpec>,
    pub screenshot: Option<PathBuf>,
    pub screenshot_after: Duration,
    pub exit_after_screenshot: bool,
}

/// The longest `--screenshot-after` waits.
const MAX_SCREENSHOT_DELAY: Duration = Duration::from_secs(24 * 3600);

impl Args {
    pub fn parse(args: impl Iterator<Item = String>) -> Self {
        let mut parsed = Args {
            screenshot_after: Duration::from_secs(10),
            ..Default::default()
        };
        let mut hosts = Vec::new();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let mut value = || args.next().unwrap_or_default();
            match arg.as_str() {
                // No faster than the app allows: every ping holds a thread until it's answered.
                "--interval" => {
                    parsed.interval = parse_secs(&value())
                        .filter(|d| !d.is_zero())
                        .map(|d| d.max(MIN_INTERVAL));
                }
                "--window" => parsed.window_secs = parse_window(&value()),
                "--theme" => parsed.theme = ThemeChoice::parse(&value()),
                // Written like a host in the add box: `ipv6:esx01`, or `10.0.0.5 Office edge`.
                "--route" => parsed.route = parse_targets(&value()).into_iter().next(),
                "--screenshot" => parsed.screenshot = Some(PathBuf::from(value())),
                "--screenshot-after" => {
                    if let Some(after) = parse_secs(&value()).filter(|d| *d <= MAX_SCREENSHOT_DELAY) {
                        parsed.screenshot_after = after;
                    }
                }
                "--exit-after-screenshot" => parsed.exit_after_screenshot = true,
                _ => hosts.push(arg),
            }
        }
        parsed.hosts = parse_targets(&hosts.join(","));
        if let Some(route) = &parsed.route
            && !parsed.hosts.iter().any(|h| h.same_host(route))
        {
            parsed.hosts.push(route.clone());
        }
        parsed
    }
}

/// Seconds, `0.5` say, as long as it's a time: not negative, infinite or too big.
fn parse_secs(text: &str) -> Option<Duration> {
    Duration::try_from_secs_f64(text.trim().parse().ok()?).ok()
}

/// `90`, `90s`, `5m`, `1h` -> seconds.
fn parse_window(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    let (number, unit) = match text.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => (&text[..i], &text[i..]),
        None => (text.as_str(), "s"),
    };
    let n: u64 = number.parse().ok()?;
    let scale = match unit {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        _ => return None,
    };
    n.checked_mul(scale).filter(|s| *s > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_flags() {
        let args = Args::parse(
            ["1.1.1.1", "--interval", "0.5", "esx01", "--window", "15m"]
                .into_iter()
                .map(String::from),
        );
        let hosts: Vec<_> = args.hosts.iter().map(|h| h.host.as_str()).collect();
        assert_eq!(hosts, ["1.1.1.1", "esx01"]);
        assert_eq!(args.interval, Some(Duration::from_millis(500)));
        assert_eq!(args.window_secs, Some(900));
    }

    #[test]
    fn route_adds_its_host_once() {
        let parse = |args: &[&str]| Args::parse(args.iter().map(|a| a.to_string()));
        let args = parse(&["esx01", "--route", "esx02"]);
        let hosts: Vec<_> = args.hosts.iter().map(|h| h.host.as_str()).collect();
        assert_eq!(hosts, ["esx01", "esx02"]);
        assert_eq!(args.route.map(|r| r.host).as_deref(), Some("esx02"));
        assert_eq!(parse(&["esx01", "--route", "ESX01"]).hosts.len(), 1);
        assert_eq!(parse(&["esx01, esx02", "--route", "esx02"]).hosts.len(), 2);

        // Written as in the add box, with a family or a label.
        let args = parse(&["esx01", "--route", "ipv6:esx01"]);
        assert_eq!(args.hosts.len(), 2, "IPv6 is another lane");
        assert_eq!(args.route.as_ref(), args.hosts.last());
        let args = parse(&["--route", "10.0.0.5 Office edge"]);
        assert_eq!(args.route.as_ref().map(|r| r.host.as_str()), Some("10.0.0.5"));
        assert_eq!(args.hosts[0].label.as_deref(), Some("Office edge"));
    }

    #[test]
    fn numbers_that_arent_times_are_ignored() {
        let parse = |args: &[&str]| Args::parse(args.iter().map(|a| a.to_string()));
        for interval in ["inf", "NaN", "-1", "0", "1e20", "soon"] {
            assert_eq!(parse(&["--interval", interval]).interval, None, "--interval {interval}");
        }
        // Faster than the app allows is as fast as it allows.
        assert_eq!(parse(&["--interval", "0.001"]).interval, Some(MIN_INTERVAL));
        for after in ["inf", "-5", "1e12"] {
            assert_eq!(
                parse(&["--screenshot-after", after]).screenshot_after,
                Duration::from_secs(10)
            );
        }
        assert_eq!(
            parse(&["--screenshot-after", "2.5"]).screenshot_after,
            Duration::from_millis(2500)
        );
    }

    #[test]
    fn window_units() {
        assert_eq!(parse_window("90"), Some(90));
        assert_eq!(parse_window("1h"), Some(3600));
        assert_eq!(parse_window("soon"), None);
        assert_eq!(parse_window("9999999999999999h"), None, "too long to count");
    }
}
