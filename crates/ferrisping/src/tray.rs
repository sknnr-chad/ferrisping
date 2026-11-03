//! The tray icon by the clock: alerts as Windows notifications, and the way back to
//! a window closed to the tray.
//!
//! On Windows the icon belongs to a thread of its own, with a hidden window that
//! gets the icon's clicks and Explorer's messages. Alerts go there straight from
//! the engine and wait a moment, so a burst of them shares one banner. The app
//! hears about clicks as [`TrayCommand`]s, and tells the tray what its tooltip and
//! menu say. Banners are tray balloons, which Windows shows as notifications
//! without FerrisPing registering anything.

#[cfg(windows)]
mod win32;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};

use eframe::egui;
use ferrisping_core::{Alert, EventKind, HostStatus, State};

use crate::{events, format};

/// Most of a banner's title that Windows shows.
const TITLE_MAX: usize = 48;
/// Most of a banner's text that Windows shows.
const TEXT_MAX: usize = 200;
/// Most of a host's name in a banner, so what happened to it still fits.
const NAME_MAX: usize = 32;
/// Hosts named in a banner about several; the rest are counted.
const NAMES_LISTED: usize = 4;

/// What the tray asks of the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    /// Open the window: the icon was clicked, or FerrisPing was started again.
    Open,
    /// Open the window with the events showing: a banner about alerts was clicked.
    OpenEvents,
    TogglePause,
    ToggleNotifications,
    Quit,
}

/// What a notification says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Banner {
    pub title: String,
    pub text: String,
}

/// What the app and the tray's thread share.
struct Shared {
    /// Alerts waiting for their banner.
    alerts: Mutex<Vec<Alert>>,
    /// A banner about something else, waiting to be shown.
    note: Mutex<Option<Banner>>,
    tip: Mutex<String>,
    notifications: AtomicBool,
    sound: AtomicBool,
    /// For the menu, which offers to pause or to resume.
    paused: AtomicBool,
    commands: mpsc::Sender<TrayCommand>,
    ctx: egui::Context,
}

impl Shared {
    fn send(&self, command: TrayCommand) {
        // Fails only once the app is gone.
        let _ = self.commands.send(command);
        // A hidden window runs the app's logic on a repaint request, too.
        self.ctx.request_repaint();
    }
}

pub struct Tray {
    shared: Arc<Shared>,
    commands: mpsc::Receiver<TrayCommand>,
    #[cfg(windows)]
    icon: Option<win32::Icon>,
}

impl Tray {
    /// Put FerrisPing's icon in the tray, or None where there's none. A second start
    /// of FerrisPing opens the `main` copy's window instead of running.
    pub fn start(ctx: &egui::Context, main: bool) -> Option<Self> {
        #[cfg(windows)]
        {
            let mut tray = Self::new(ctx);
            tray.icon = Some(win32::Icon::start(Arc::clone(&tray.shared), main)?);
            Some(tray)
        }
        #[cfg(not(windows))]
        {
            let _ = (ctx, main);
            None
        }
    }

    fn new(ctx: &egui::Context) -> Self {
        let (sender, commands) = mpsc::channel();
        Self {
            shared: Arc::new(Shared {
                alerts: Mutex::new(Vec::new()),
                note: Mutex::new(None),
                tip: Mutex::new("FerrisPing".to_string()),
                notifications: AtomicBool::new(true),
                sound: AtomicBool::new(true),
                paused: AtomicBool::new(false),
                commands: sender,
                ctx: ctx.clone(),
            }),
            commands,
            #[cfg(windows)]
            icon: None,
        }
    }

    /// A tray without an icon, for tests: commands come from the test, and alerts
    /// and banners wait for it to look.
    #[cfg(test)]
    pub fn detached(ctx: &egui::Context) -> Self {
        Self::new(ctx)
    }

    /// What was asked since the last call.
    pub fn commands(&self) -> Vec<TrayCommand> {
        self.commands.try_iter().collect()
    }

    /// Hands each alert to the tray, for [`ferrisping_core::Monitor::set_on_alert`].
    pub fn notifier(&self) -> impl Fn(Alert) + Send + Sync + 'static {
        let shared = Arc::clone(&self.shared);
        #[cfg(windows)]
        let window = self.icon.as_ref().map(win32::Icon::window);
        move |alert| {
            if !shared.notifications.load(Ordering::Relaxed) {
                return;
            }
            lock(&shared.alerts).push(alert);
            #[cfg(windows)]
            if let Some(window) = window {
                window.alerts();
            }
        }
    }

    /// Whether alerts show as notifications, and play Windows' sound.
    pub fn set_options(&self, notifications: bool, sound: bool) {
        self.shared.notifications.store(notifications, Ordering::Relaxed);
        self.shared.sound.store(sound, Ordering::Relaxed);
        if !notifications {
            lock(&self.shared.alerts).clear();
        }
    }

    /// The icon's tooltip, and whether pinging is paused.
    pub fn set_status(&self, tip: &str, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
        let mut current = lock(&self.shared.tip);
        if *current != tip {
            tip.clone_into(&mut current);
            drop(current);
            #[cfg(windows)]
            if let Some(icon) = &self.icon {
                icon.window().tip();
            }
        }
    }

    /// Show a banner that isn't about alerts, even with alerts' notifications off.
    pub fn note(&self, banner: Banner) {
        *lock(&self.shared.note) = Some(banner);
        #[cfg(windows)]
        if let Some(icon) = &self.icon {
            icon.window().note();
        }
    }

    #[cfg(test)]
    pub fn send_for_test(&self, command: TrayCommand) {
        self.shared.send(command);
    }

    #[cfg(test)]
    pub fn take_note_for_test(&self) -> Option<Banner> {
        lock(&self.shared.note).take()
    }

    #[cfg(test)]
    pub fn tip_for_test(&self) -> String {
        lock(&self.shared.tip).clone()
    }
}

/// Whether this is the only copy of FerrisPing started normally, rather than for a
/// session of hosts from the command line. If another one is running, it's asked to
/// open its window. Where there's no tray, every copy runs.
pub fn first_copy() -> bool {
    #[cfg(windows)]
    {
        win32::claim(win32::MUTEX) || !win32::open_running_copy()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// The banner shown the first time the window closes to the tray.
pub fn still_running() -> Banner {
    Banner {
        title: "FerrisPing is still running".to_string(),
        text: "It keeps pinging from the tray by the clock, under ^ if the crab isn't showing there. Click the crab \
               to open FerrisPing again, or right-click it to quit."
            .to_string(),
    }
}

/// The icon's tooltip: `FerrisPing · 6 hosts · 1 down`.
pub fn tooltip(state: &State) -> String {
    let mut parts = vec!["FerrisPing".to_string()];
    parts.push(match state.hosts.len() {
        0 => "no hosts".to_string(),
        1 => "1 host".to_string(),
        n => format!("{n} hosts"),
    });
    let down = state.hosts.iter().filter(|h| h.status == HostStatus::Down).count();
    if state.settings.paused {
        parts.push("paused".to_string());
    } else if down > 0 {
        parts.push(format!("{down} down"));
    }
    parts.join(" · ")
}

/// One banner for a burst of alerts: what happened to a host, or to how many, or a
/// sentence for each kind of alert when there are several.
pub fn banner(alerts: &[Alert]) -> Banner {
    // What happened, with the hosts it happened to, in the order the alerts came.
    let mut groups: Vec<(Phrase, Vec<&str>)> = Vec::new();
    for alert in alerts {
        let phrase = phrase(alert);
        let name = alert.event.host_name.as_str();
        match groups.iter_mut().find(|(seen, _)| *seen == phrase) {
            Some((_, names)) if names.contains(&name) => {}
            Some((_, names)) => names.push(name),
            None => groups.push((phrase, vec![name])),
        }
    }
    let banner = match groups.as_slice() {
        [] => Banner {
            title: String::new(),
            text: String::new(),
        },
        [(phrase, names)] if names.len() == 1 => {
            let alert = alerts
                .iter()
                .find(|alert| alert.event.host_name == names[0])
                .expect("the name came from an alert");
            // Shorten a long name rather than what happened to it.
            let room = TITLE_MAX
                .saturating_sub(phrase.one.chars().count() + 1)
                .clamp(12, NAME_MAX);
            Banner {
                title: format!("{} {}", cut(names[0], room), phrase.one),
                text: detail(alert),
            }
        }
        [(phrase, names)] => Banner {
            title: format!("{} hosts {}", names.len(), phrase.several),
            text: list(names),
        },
        _ => Banner {
            title: format!("{} alerts", alerts.len()),
            text: groups
                .iter()
                .map(|(phrase, names)| {
                    let verb = if names.len() == 1 { &phrase.one } else { &phrase.several };
                    format!("{} {verb}.", list(names))
                })
                .collect::<Vec<_>>()
                .join(" "),
        },
    };
    Banner {
        title: cut(&banner.title, TITLE_MAX),
        text: cut(&banner.text, TEXT_MAX),
    }
}

/// What happened to a host, after its name: for one host, and for several.
#[derive(PartialEq)]
struct Phrase {
    one: String,
    several: String,
}

fn phrase(alert: &Alert) -> Phrase {
    let (one, several) = match &alert.event.kind {
        EventKind::Down => ("stopped answering", "stopped answering"),
        EventKind::Up { .. } => ("is answering again", "are answering again"),
        EventKind::Loss { .. } => ("is losing pings", "are losing pings"),
        EventKind::LossEnded { .. } => ("is no longer losing pings", "are no longer losing pings"),
        EventKind::Slow { .. } => ("is slow", "are slow"),
        EventKind::SlowEnded { .. } => ("is no longer slow", "are no longer slow"),
        EventKind::ResolveFailed(_) => ("can't be found", "can't be found"),
        EventKind::ExitChanged { .. } => {
            let to = alert.networks.as_ref().map_or("another network", |(_, to)| to.as_str());
            return Phrase {
                one: format!("now leaves via {to}"),
                several: format!("now leave via {to}"),
            };
        }
    };
    Phrase {
        one: one.to_string(),
        several: several.to_string(),
    }
}

/// The text of a banner about one alert.
fn detail(alert: &Alert) -> String {
    if let Some(detail) = events::limit_detail(&alert.event.kind) {
        return detail;
    }
    match &alert.event.kind {
        EventKind::Down => match alert.addr {
            Some(addr) if addr.to_string() != alert.event.host_name => format!("No replies from {addr}."),
            _ => "No replies.".to_string(),
        },
        EventKind::Up { outage_secs } => format!("After {} without replies.", format::duration(*outage_secs)),
        EventKind::ResolveFailed(error) => error.clone(),
        EventKind::ExitChanged { .. } => match &alert.networks {
            Some((from, _)) => format!("Its traffic left via {from} before."),
            None => String::new(),
        },
        _ => String::new(),
    }
}

/// `db-01`, `db-01 and db-02`, `db-01, db-02 and web-03`, `db-01, … and 3 more`.
fn list(names: &[&str]) -> String {
    let names: Vec<String> = names.iter().map(|name| cut(name, NAME_MAX)).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [first @ .., last] if names.len() <= NAMES_LISTED => format!("{} and {last}", first.join(", ")),
        _ => format!(
            "{} and {} more",
            names[..NAMES_LISTED - 1].join(", "),
            names.len() - (NAMES_LISTED - 1)
        ),
    }
}

/// `text`, cut with an ellipsis to at most `max` characters.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max - 1).collect();
    format!("{}…", kept.trim_end())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use ferrisping_core::{Event, HostId};

    use super::*;

    fn alert(name: &str, kind: EventKind) -> Alert {
        Alert {
            event: Event {
                at: 0.0,
                host: HostId(1),
                host_name: name.to_string(),
                kind,
            },
            addr: Some("192.0.2.10".parse::<IpAddr>().unwrap()),
            networks: None,
        }
    }

    fn banner_of(alerts: &[Alert]) -> (String, String) {
        let Banner { title, text } = banner(alerts);
        (title, text)
    }

    #[test]
    fn one_alert_says_what_happened_to_its_host() {
        assert_eq!(
            banner_of(&[alert("db-01", EventKind::Down)]),
            ("db-01 stopped answering".into(), "No replies from 192.0.2.10.".into())
        );
        assert_eq!(
            banner_of(&[alert("192.0.2.10", EventKind::Down)]),
            ("192.0.2.10 stopped answering".into(), "No replies.".into())
        );
        assert_eq!(
            banner_of(&[alert("db-01", EventKind::Up { outage_secs: 456.0 })]),
            (
                "db-01 is answering again".into(),
                "After 7m 36s without replies.".into()
            )
        );
        let loss = EventKind::Loss {
            lost: 4,
            pings: 30,
            limit_pct: 10.0,
        };
        assert_eq!(
            banner_of(&[alert("db-01", loss)]),
            (
                "db-01 is losing pings".into(),
                "4 of the last 30 pings got no reply. The limit is 10%.".into()
            )
        );
        let slow = EventKind::Slow {
            avg_ms: 245.0,
            pings: 30,
            limit_ms: 200.0,
        };
        assert_eq!(
            banner_of(&[alert("db-01", slow)]).1,
            "Replies averaged 245 ms over the last 30 pings. The limit is 200 ms."
        );
        let switch = Alert {
            networks: Some(("Lumen".into(), "Comcast".into())),
            ..alert("db-01", EventKind::ExitChanged { from: 3356, to: 7922 })
        };
        assert_eq!(
            banner_of(&[switch]),
            (
                "db-01 now leaves via Comcast".into(),
                "Its traffic left via Lumen before.".into()
            )
        );
    }

    #[test]
    fn a_burst_of_one_kind_counts_its_hosts() {
        let alerts = ["db-01", "db-02", "web-03"].map(|name| alert(name, EventKind::Down));
        assert_eq!(
            banner_of(&alerts),
            ("3 hosts stopped answering".into(), "db-01, db-02 and web-03".into())
        );
        let many: Vec<Alert> = (1..=9).map(|n| alert(&format!("db-0{n}"), EventKind::Down)).collect();
        assert_eq!(banner_of(&many).1, "db-01, db-02, db-03 and 6 more");
    }

    #[test]
    fn a_mixed_burst_has_a_sentence_for_each_kind() {
        let alerts = [
            alert("db-01", EventKind::Down),
            alert(
                "web-03",
                EventKind::Slow {
                    avg_ms: 250.0,
                    pings: 30,
                    limit_ms: 200.0,
                },
            ),
            alert("db-02", EventKind::Down),
        ];
        assert_eq!(
            banner_of(&alerts),
            (
                "3 alerts".into(),
                "db-01 and db-02 stopped answering. web-03 is slow.".into()
            )
        );
    }

    #[test]
    fn long_names_leave_room_for_what_happened() {
        let name = "a-very-long-host-name.branch-office.example.com";
        let (title, _) = banner_of(&[alert(name, EventKind::Down)]);
        assert_eq!(title, "a-very-long-host-name.branch-… stopped answering");
        assert!(title.chars().count() <= TITLE_MAX);
        let many: Vec<Alert> = (0..20)
            .map(|n| alert(&format!("{name}-{n}"), EventKind::Down))
            .collect();
        let (_, text) = banner_of(&many);
        assert!(text.chars().count() <= TEXT_MAX, "{text}");
    }

    #[test]
    fn the_still_running_banner_fits() {
        let Banner { title, text } = still_running();
        assert!(title.chars().count() <= TITLE_MAX);
        assert!(text.chars().count() <= TEXT_MAX, "{} characters", text.chars().count());
    }
}
