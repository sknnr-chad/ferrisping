//! Trace a route for a while and print a pathping-style report.
//!
//! `cargo run --release -p ferrisping-core --example trace -- 1.1.1.1 20`

use std::net::IpAddr;
use std::time::Duration;

use ferrisping_core::{Monitor, ProbeSettings, TraceSettings};

fn main() {
    let mut args = std::env::args().skip(1);
    let target: IpAddr = args.next().expect("target IP address").parse().expect("IP address");
    let secs: u64 = args.next().map_or(20, |s| s.parse().expect("seconds"));

    let monitor = Monitor::new(ProbeSettings::default());
    let trace = monitor.trace(target, TraceSettings::default());
    std::thread::sleep(Duration::from_secs(secs));

    let route = trace.read();
    let reached = match route.reached_at {
        Some(hops) => format!("reached in {hops} hops"),
        None => "not reached".to_string(),
    };
    println!("Route to {target} after {secs} s: {reached}");
    println!(
        "{:>3}  {:<46} {:>5} {:>6} {:>7} {:>7} {:>7} {:>7} {:>7}",
        "Hop", "Host", "Sent", "Loss", "Last", "Avg", "Best", "Worst", "Jitter"
    );
    let ms = |v: Option<f32>| v.map_or("-".to_string(), |v| format!("{v:.1}"));
    for (i, hop) in route.shown_hops().iter().enumerate() {
        let host = match hop.addr() {
            Some(addr) => {
                let name = route.name(addr);
                let mut host = if name == addr.to_string() {
                    name
                } else {
                    format!("{name} [{addr}]")
                };
                if hop.addrs.len() > 1 {
                    host.push_str(&format!(" +{}", hop.addrs.len() - 1));
                }
                host
            }
            None => "*".to_string(),
        };
        println!(
            "{:>3}  {:<46} {:>5} {:>5}% {:>7} {:>7} {:>7} {:>7} {:>7}{}",
            i + 1,
            host,
            hop.sent,
            hop.loss_pct().map_or("-".into(), |l| format!("{l:.0}")),
            ms(hop.last_ms),
            ms(hop.avg_ms()),
            ms(hop.best_ms),
            ms(hop.worst_ms),
            ms(hop.jitter_ms()),
            if hop.unreachable { "  unreachable" } else { "" },
        );
    }
}
