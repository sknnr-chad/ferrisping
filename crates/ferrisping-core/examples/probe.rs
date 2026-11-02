//! Compare Windows' whole-millisecond round trip with our own timer.
//!
//! `cargo run --release -p ferrisping-core --example probe -- 127.0.0.1 192.168.1.1 1.1.1.1`

use std::net::IpAddr;
use std::time::Duration;

fn main() {
    let count = 40;
    for target in std::env::args().skip(1) {
        let ip: IpAddr = target.parse().expect("IP address");
        let mut pairs = Vec::new();
        for _ in 0..count {
            let r = ferrisping_core::echo(ip, Duration::from_secs(2), 128, 32);
            if r.status.is_reply() {
                pairs.push((r.windows_ms, r.elapsed_ms));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut diffs: Vec<f64> = pairs.iter().map(|(w, e)| e - *w as f64).collect();
        diffs.sort_by(f64::total_cmp);
        let mut elapsed: Vec<f64> = pairs.iter().map(|(_, e)| *e).collect();
        elapsed.sort_by(f64::total_cmp);
        let pct = |v: &[f64], p: f64| {
            v.get(((v.len() as f64 - 1.0) * p).round() as usize)
                .copied()
                .unwrap_or(f64::NAN)
        };
        println!("{target}: {} replies of {count}", pairs.len());
        println!(
            "  ours ms      min {:.3}  p50 {:.3}  p90 {:.3}  max {:.3}",
            pct(&elapsed, 0.0),
            pct(&elapsed, 0.5),
            pct(&elapsed, 0.9),
            pct(&elapsed, 1.0)
        );
        println!(
            "  ours-windows min {:.3}  p50 {:.3}  p90 {:.3}  max {:.3}",
            pct(&diffs, 0.0),
            pct(&diffs, 0.5),
            pct(&diffs, 0.9),
            pct(&diffs, 1.0)
        );
        let sample: Vec<String> = pairs.iter().take(12).map(|(w, e)| format!("{w}/{e:.2}")).collect();
        println!("  windows/ours: {}", sample.join("  "));
    }
}
