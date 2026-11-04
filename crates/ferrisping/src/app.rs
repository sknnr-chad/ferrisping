//! The FerrisPing window: toolbar, host lanes, outage log and status bar.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Button, Color32, ComboBox, CornerRadius, Frame, Key, Layout, Margin, RichText, Stroke, TextEdit, Ui,
    vec2,
};
use ferrisping_core::{
    Host, HostId, HostStatus, Monitor, ProbeSettings, State, TargetSpec, TraceSettings, now_secs, parse_targets,
};
use serde::{Deserialize, Serialize};

use crate::add::{self, Adding};
use crate::alerts::{AlertChoices, AlertsDialog, HostAlertsDialog, HostChoice, Outcome};
use crate::args::Args;
use crate::events;
use crate::format::{self, Clock};
use crate::lanes::{self, LaneAction, View};
use crate::route::{self, RouteAction, RouteOutput, RouteView};
use crate::route_history::{self, RouteHistory};
use crate::theme::{self, Palette, ThemeChoice};
use crate::tray::{self, Tray, TrayCommand};

#[cfg(test)]
mod tests;

const STORAGE_KEY: &str = "ferrisping";
const INTERVALS_MS: [u64; 8] = [250, 500, 1000, 2000, 5000, 10_000, 30_000, 60_000];
/// The shortest ping interval taken from saved settings or the command line.
pub const MIN_INTERVAL: Duration = Duration::from_millis(100);
const WINDOWS: [(u64, &str); 6] = [
    (60, "1m"),
    (300, "5m"),
    (900, "15m"),
    (3600, "1h"),
    (21_600, "6h"),
    (86_400, "24h"),
];
/// Room right of the newest sample in live mode, as a fraction of the window.
const LIVE_PAD: f64 = 0.015;
const QUICK_HOSTS: [&str; 3] = ["1.1.1.1", "8.8.8.8", "google.com"];
/// The route panel at its smallest: its title, the column names and a row.
const ROUTE_MIN_HEIGHT: f32 = 150.0;
/// How much of the room a route panel takes, at most, to fit its route by itself.
const ROUTE_FIT_SHARE: f32 = 0.55;
/// Room the lanes keep when the route panel is dragged taller.
const LANES_MIN_HEIGHT: f32 = 180.0;
/// Room the lanes keep when the Events panel is dragged wider: what they have beside
/// it at its usual width in the narrowest window.
const LANES_MIN_WIDTH: f32 = 420.0;
/// How long the status bar says probes are being skipped after the last one was.
const SKIPPING_SHOWN_SECS: f64 = 10.0;
/// What FerrisPing sends over the network, in full.
const PRIVACY_URL: &str = "https://ferrisping.com/#privacy";
/// The setting's name, in the notice and the menu.
const LOOKUPS_LABEL: &str = "Check routes and look up networks";
/// The jitter strip's setting, in the menu.
const JITTER_LABEL: &str = "Show jitter under graphs";

/// What survives a restart.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    hosts: Vec<String>,
    interval_ms: u64,
    window_secs: u64,
    theme: ThemeChoice,
    show_events: bool,
    /// Show jitter in a strip under each graph.
    show_jitter: bool,
    /// Names the user gave networks, by AS number.
    network_names: BTreeMap<u32, String>,
    /// Check each host's route, and ask Team Cymru who runs the routers on a route.
    network_lookups: bool,
    /// The notice about what FerrisPing sends over the network has been answered.
    notice_answered: bool,
    /// Which events raise alerts, and how alerts show.
    alerts: AlertChoices,
    /// Hosts with alerts of their own, by their line in `hosts`, name and all.
    host_alerts: BTreeMap<String, HostChoice>,
    /// ✕ hides the window in the tray, and FerrisPing keeps pinging.
    close_to_tray: bool,
    /// The banner saying FerrisPing still runs in the tray has been shown.
    tray_tip_shown: bool,
    /// Hosts added or removed before, as typed and newest first, to add again.
    recent_hosts: Vec<String>,
}

impl Default for Saved {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            interval_ms: 1000,
            window_secs: 300,
            theme: ThemeChoice::System,
            show_events: true,
            show_jitter: false,
            network_names: BTreeMap::new(),
            network_lookups: true,
            notice_answered: false,
            alerts: AlertChoices::default(),
            host_alerts: BTreeMap::new(),
            close_to_tray: false,
            tray_tip_shown: false,
            recent_hosts: Vec::new(),
        }
    }
}

/// The notice about what FerrisPing sends over the network, while it's open. It
/// opens by itself on the first run, before anything optional is sent.
struct Notice {
    /// Whether to check routes and look up networks, as ticked in the notice.
    lookups: bool,
    /// It opened by itself on the first run, so only OK closes it. Read again from
    /// the menu, it can be closed without changing anything.
    first_run: bool,
}

/// The dialog that names a network or a host, while it's open.
struct Rename {
    what: Renaming,
    text: String,
    /// The user named it before, so it can go back to its usual name.
    renamed: bool,
    /// Not drawn yet: focus the name and select it on the first frame.
    fresh: bool,
}

/// What the rename dialog names.
enum Renaming {
    /// A network by AS number, with what the registry calls it, shown for reference.
    Network { asn: u32, registered: Option<String> },
    /// A host, with its address, shown for reference.
    Host { id: HostId, address: String },
}

/// The crab at two sizes: the renderer can't shrink one image far without shimmer.
struct Logo {
    /// 64 px, for the toolbar.
    small: egui::TextureHandle,
    /// 256 px, for the empty window.
    large: egui::TextureHandle,
}

impl Logo {
    fn load(ctx: &egui::Context) -> Self {
        Self {
            small: texture(ctx, "logo-64", include_bytes!("../assets/ferrisping-64.png")),
            large: texture(ctx, "logo-256", include_bytes!("../assets/ferrisping-256.png")),
        }
    }
}

fn texture(ctx: &egui::Context, name: &str, png: &[u8]) -> egui::TextureHandle {
    let rgba = image::load_from_memory(png)
        .expect("the embedded logo is a valid PNG")
        .into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    ctx.load_texture(name, image, egui::TextureOptions::LINEAR)
}

struct Screenshot {
    path: PathBuf,
    at: Instant,
    exit: bool,
    requested: bool,
}

/// Changes collected while drawing and applied once the state lock is released.
enum Action {
    Add(Vec<TargetSpec>),
    Lane(LaneAction),
    Route(RouteAction),
    SetInterval(Duration),
    SetNetworkLookups(bool),
    TogglePause,
    ClearData,
}

pub struct FerrisPingApp {
    monitor: Arc<Monitor>,
    /// How soon to redraw after new samples arrive, in ms (shared with the engine callback).
    redraw_ms: Arc<AtomicU64>,
    clock: Clock,
    adding: Adding,
    /// The toolbar has only the Add button, as the window is too narrow for the box.
    add_folded: bool,
    toolbar_widths: ToolbarWidths,
    window_secs: u64,
    live: bool,
    /// Visible time range the graphs reported last frame.
    bounds: Option<(f64, f64)>,
    theme: ThemeChoice,
    show_events: bool,
    show_jitter: bool,
    /// The traceroute in the panel under the lanes, while it's open.
    route: Option<RouteView>,
    /// The route panel's height once the user drags its edge; until then it fits the route.
    route_height: Option<f32>,
    /// The height that shows the whole route, measured last frame.
    route_fit: f32,
    /// A host named with `--route`, traced once its address is known.
    route_on_start: Option<TargetSpec>,
    /// The routes a host's traffic took in a stretch of time picked on its graph,
    /// shown in the route panel's place while it's open.
    routes_seen: Option<RouteHistory>,
    rename: Option<Rename>,
    notice: Option<Notice>,
    /// The notice has been answered, on this run or an earlier one.
    notice_answered: bool,
    alerts: AlertChoices,
    alerts_dialog: Option<AlertsDialog>,
    host_alerts_dialog: Option<HostAlertsDialog>,
    /// The tray icon, if there is one. Pictures for docs and tests go without.
    tray: Option<Tray>,
    close_to_tray: bool,
    tray_tip_shown: bool,
    /// Quitting for real, so a close doesn't hide the window in the tray.
    quitting: bool,
    /// Hidden in the tray, where only the tooltip shows how hosts are doing.
    hidden: bool,
    logo: Logo,
    /// Hosts came from the command line: a one-off session that isn't saved.
    session: bool,
    screenshot: Option<Screenshot>,
}

impl FerrisPingApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        let saved = cc
            .storage
            .and_then(|storage| eframe::get_value(storage, STORAGE_KEY))
            .unwrap_or_default();
        Self::with_saved(cc, args, saved)
    }

    /// The app as `saved` left it, with `args` on top.
    fn with_saved(cc: &eframe::CreationContext<'_>, args: Args, saved: Saved) -> Self {
        let theme = args.theme.unwrap_or(saved.theme);
        theme::install(&cc.egui_ctx, theme);

        let session = !args.hosts.is_empty();
        // The notice opens by itself on the first normal run. One-off sessions and
        // pictures for docs start without it.
        let notice = (!saved.notice_answered && !session && args.screenshot.is_none()).then_some(Notice {
            lookups: saved.network_lookups,
            first_run: true,
        });
        let interval = args
            .interval
            .unwrap_or(Duration::from_millis(saved.interval_ms).max(MIN_INTERVAL));
        let monitor = Arc::new(Monitor::new(ProbeSettings {
            interval,
            // Until the notice is answered, routes aren't checked and nothing is looked
            // up, in a one-off session too. Pictures for docs go on as before.
            network_lookups: saved.network_lookups && (saved.notice_answered || args.screenshot.is_some()),
            ..ProbeSettings::default()
        }));
        let ctx = cc.egui_ctx.clone();
        let window_secs = args.window_secs.unwrap_or(saved.window_secs);
        let redraw_ms = Arc::new(AtomicU64::new(redraw_interval(window_secs).as_millis() as u64));
        let delay = Arc::clone(&redraw_ms);
        monitor.set_on_change(move || {
            ctx.request_repaint_after(Duration::from_millis(delay.load(Ordering::Relaxed)));
        });
        for (&asn, name) in &saved.network_names {
            monitor.set_network_name(asn, Some(name.clone()));
        }
        monitor.set_alerts(saved.alerts.settings());

        let hosts = if session {
            args.hosts.clone()
        } else {
            saved.hosts.iter().filter_map(|h| TargetSpec::from_input(h)).collect()
        };
        add_hosts(&monitor, hosts);
        let own: Vec<(HostId, HostChoice)> = monitor
            .read()
            .hosts
            .iter()
            .filter_map(|h| saved.host_alerts.get(&h.spec.to_input()).map(|&choice| (h.id, choice)))
            .collect();
        for (id, choice) in own {
            monitor.set_host_alerts(id, choice.alerts());
        }

        Self {
            monitor,
            redraw_ms,
            clock: Clock::new(),
            adding: Adding::new(saved.recent_hosts),
            add_folded: false,
            toolbar_widths: ToolbarWidths::default(),
            window_secs,
            live: true,
            bounds: None,
            theme,
            show_events: saved.show_events,
            show_jitter: saved.show_jitter,
            route: None,
            route_height: None,
            route_fit: ROUTE_MIN_HEIGHT,
            route_on_start: args.route,
            routes_seen: None,
            rename: None,
            notice,
            notice_answered: saved.notice_answered,
            alerts: saved.alerts,
            alerts_dialog: None,
            host_alerts_dialog: None,
            tray: None,
            close_to_tray: saved.close_to_tray,
            tray_tip_shown: saved.tray_tip_shown,
            quitting: false,
            hidden: false,
            logo: Logo::load(&cc.egui_ctx),
            session,
            screenshot: args.screenshot.map(|path| Screenshot {
                path,
                at: Instant::now() + args.screenshot_after,
                exit: args.exit_after_screenshot,
                requested: false,
            }),
        }
    }

    /// Show alerts from the tray icon, and take its clicks.
    pub fn attach_tray(&mut self, tray: Tray) {
        tray.set_options(self.alerts.notifications, self.alerts.sound);
        self.monitor.set_on_alert(tray.notifier());
        self.tray = Some(tray);
    }

    /// Bring the window back from the tray or the taskbar, in front.
    fn open_window(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Do what the tray asked, and keep it up to date: its tooltip, its menu, and
    /// whether alerts show.
    fn tray_logic(&mut self, ctx: &egui::Context) {
        let Some(commands) = self.tray.as_ref().map(Tray::commands) else {
            return;
        };
        for command in commands {
            match command {
                TrayCommand::Open => self.open_window(ctx),
                TrayCommand::OpenEvents => {
                    self.show_events = true;
                    self.open_window(ctx);
                }
                TrayCommand::TogglePause => self.monitor.update_settings(|s| s.paused = !s.paused),
                TrayCommand::ToggleNotifications => self.alerts.notifications = !self.alerts.notifications,
                TrayCommand::Quit => self.quit(ctx),
            }
        }
        let Some(tray) = &self.tray else {
            return;
        };
        tray.set_options(self.alerts.notifications, self.alerts.sound);
        // ✕, or Alt+F4: hide in the tray rather than quit, if that's what's wanted.
        if self.close_to_tray && !self.quitting && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
            if !self.tray_tip_shown {
                tray.note(tray::still_running());
                self.tray_tip_shown = true;
            }
        }
        let state = self.monitor.read();
        let tip = tray::tooltip(&state);
        let paused = state.settings.paused;
        drop(state);
        tray.set_status(&tip, paused);
    }

    fn toolbar(
        &mut self,
        ui: &mut Ui,
        p: &Palette,
        settings: &ProbeSettings,
        hosts: &[Host],
        actions: &mut Vec<Action>,
    ) {
        egui::Panel::top("toolbar")
            .frame(Frame::new().fill(p.surface).inner_margin(Margin::symmetric(16, 10)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new((self.logo.small.id(), vec2(24.0, 24.0))));
                    ui.label(
                        RichText::new("FerrisPing")
                            .font(theme::font_semibold(17.0))
                            .color(p.text),
                    );
                    ui.add_space(16.0);

                    // What fits comes from widths measured in earlier frames. When one
                    // changes, the frame is drawn again before it's shown.
                    let fit = self
                        .toolbar_widths
                        .fit(ui.available_width(), ui.spacing().item_spacing.x);
                    let mut widths = self.toolbar_widths;
                    let out = add::show(ui, &mut self.adding, p, fit.add, hosts);
                    self.add_folded = fit.add == add::Form::Button;
                    widths.add = out.add_width;
                    if let Some(more) = out.more_width {
                        widths.more = more;
                    }
                    for input in out.inputs {
                        self.adding.remember(&input);
                        actions.push(Action::Add(parse_targets(&input)));
                    }

                    // The right-hand group is laid out right to left.
                    let controls = ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        self.overflow_menu(ui, p, settings, actions);
                        let (glyph, fallback, label) = if settings.paused {
                            (theme::ICON_PLAY, "▶", "Resume")
                        } else {
                            (theme::ICON_PAUSE, "⏸", "Pause")
                        };
                        let pause = Button::new(format!("{}  {label}", theme::icon(glyph, fallback)));
                        if ui.add(pause).accessible_name(label).clicked() {
                            actions.push(Action::TogglePause);
                        }
                        if self.live {
                            live_badge(ui, p, settings.paused);
                        } else if ui
                            .add(accent_button(p, "Back to live"))
                            .on_hover_text("Follow the newest samples again (or double-click a graph)")
                            .clicked()
                        {
                            self.live = true;
                        }
                        ui.add_space(8.0);
                        let changed = if fit.spans_menu {
                            spans_menu(ui, &mut self.window_secs, &WINDOWS)
                        } else {
                            segmented(ui, p, &mut self.window_secs, &WINDOWS)
                        };
                        if changed {
                            self.live = true;
                        }
                        ui.add_space(8.0);
                        interval_picker(ui, settings, actions);
                    });
                    if !fit.spans_menu {
                        widths.controls = widths.controls.max(controls.response.rect.width());
                    }
                    if !widths.same_as(&self.toolbar_widths) {
                        self.toolbar_widths = widths;
                        ui.ctx().request_discard("the toolbar measured its controls");
                    }
                });
            });
    }

    fn overflow_menu(&mut self, ui: &mut Ui, p: &Palette, settings: &ProbeSettings, actions: &mut Vec<Action>) {
        ui.menu_button(theme::icon(theme::ICON_MORE, "⋯"), |ui| {
            ui.set_min_width(200.0);
            ui.label(RichText::new("Theme").size(12.0).color(p.text_muted));
            for choice in ThemeChoice::ALL {
                if ui.radio(self.theme == choice, choice.label()).clicked() {
                    self.theme = choice;
                    ui.ctx().set_theme(choice.preference());
                }
            }
            ui.separator();
            ui.checkbox(&mut self.show_jitter, JITTER_LABEL).on_hover_text(
                "Add a strip under each graph for jitter, on a scale of its own: how much the round \
                 trip changes from one reply to the next, on average. With many hosts, lanes are too \
                 short for it, and hovering a graph shows jitter instead.",
            );
            let mut lookups = settings.network_lookups;
            if ui
                .checkbox(&mut lookups, LOOKUPS_LABEL)
                .on_hover_text(
                    "Find the network each host's traffic leaves through and keep its routes, checking every \
                     30 seconds, and ask Team Cymru who runs the routers on a route",
                )
                .changed()
            {
                actions.push(Action::SetNetworkLookups(lookups));
            }
            if ui.button("What FerrisPing sends…").clicked() {
                self.notice = Some(Notice {
                    lookups: settings.network_lookups,
                    first_run: false,
                });
                ui.close();
            }
            ui.separator();
            if ui
                .button("Alerts…")
                .on_hover_text("What shows a Windows notification, and limits on loss and round trips")
                .clicked()
            {
                self.alerts_dialog = Some(AlertsDialog::new(self.alerts));
                ui.close();
            }
            if self.tray.is_some() {
                ui.checkbox(&mut self.close_to_tray, "Keep running in the tray when closed")
                    .on_hover_text(
                        "✕ hides the window and FerrisPing keeps pinging. Click its icon by the clock to open \
                         it again.",
                    );
            }
            ui.separator();
            if ui.button("Clear graphs and events").clicked() {
                actions.push(Action::ClearData);
                ui.close();
            }
            if self.tray.is_some() && self.close_to_tray {
                ui.separator();
                if ui.button("Quit FerrisPing").clicked() {
                    let ctx = ui.ctx().clone();
                    self.quit(&ctx);
                    ui.close();
                }
            }
        })
        .response
        .on_hover_text("More options")
        .accessible_name("More options");
    }

    fn status_bar(&mut self, ui: &mut Ui, p: &Palette, state: &State, settings: &ProbeSettings, now: f64) {
        egui::Panel::bottom("status")
            .exact_size(34.0)
            .frame(Frame::new().fill(p.surface).inner_margin(Margin::symmetric(12, 0)))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let label = match state.events.len() {
                        0 => "Events".to_string(),
                        n => format!("Events  {n}"),
                    };
                    if ui.add(Button::selectable(self.show_events, label)).clicked() {
                        self.show_events = !self.show_events;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(summary(state, settings)).size(12.5).color(p.text_muted));
                        if state.probes_skipped_at.is_some_and(|t| now - t < SKIPPING_SHOWN_SECS) {
                            ui.add_space(12.0);
                            skipping_badge(ui, p);
                        }
                    });
                });
            });
    }

    fn empty_state(&mut self, ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.2).max(24.0));
            ui.add(egui::Image::new((self.logo.large.id(), vec2(112.0, 112.0))));
            ui.add_space(10.0);
            ui.label(
                RichText::new("Nothing to watch yet")
                    .font(theme::font_semibold(22.0))
                    .color(p.text),
            );
            ui.add_space(6.0);
            let how = if self.add_folded {
                "Click Add above and type a host name or IP address."
            } else {
                "Type a host name or IP address in the box above."
            };
            ui.label(
                RichText::new(format!(
                    "{how} Add several at once with commas, a range like 10.0.0.1-20, or a \
                     subnet like 10.0.0.0/24."
                ))
                .color(p.text_secondary),
            );
            ui.add_space(18.0);
            ui.label(RichText::new("Or start with").size(12.0).color(p.text_muted));
            ui.add_space(4.0);

            // Center the row of chips by measuring it first.
            let font = egui::TextStyle::Button.resolve(ui.style());
            let pad = ui.spacing().button_padding.x * 2.0;
            let gap = ui.spacing().item_spacing.x;
            let widths: Vec<f32> = QUICK_HOSTS
                .iter()
                .map(|h| {
                    ui.painter()
                        .layout_no_wrap(h.to_string(), font.clone(), Color32::WHITE)
                        .size()
                        .x
                        + pad
                })
                .collect();
            let total = widths.iter().sum::<f32>() + gap * (widths.len() - 1) as f32;
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                for host in QUICK_HOSTS {
                    if ui.button(host).clicked() {
                        self.adding.remember(host);
                        actions.push(Action::Add(parse_targets(host)));
                    }
                }
            });
        });
    }

    fn view(&self, now: f64, settings: &ProbeSettings) -> View {
        let window = self.window_secs as f64;
        let (t0, t1) = match self.bounds {
            Some(bounds) if !self.live => bounds,
            _ => (now - window, now + window * LIVE_PAD),
        };
        View {
            now,
            t0,
            t1,
            live: self.live,
            interval: settings.interval.as_secs_f64(),
            jitter: self.show_jitter,
            span: self.routes_seen.as_ref().map(|seen| (seen.host, seen.t0, seen.t1)),
        }
    }

    fn apply(&mut self, ctx: &egui::Context, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Add(specs) => add_hosts(&self.monitor, specs),
                Action::Lane(LaneAction::Remove(id)) => {
                    if self.route.as_ref().is_some_and(|view| view.host == id) {
                        self.route = None;
                    }
                    if self.routes_seen.as_ref().is_some_and(|seen| seen.host == id) {
                        self.routes_seen = None;
                    }
                    // Kept with the hosts added before, to add again.
                    let input = self.monitor.read().host(id).map(|h| h.spec.to_input());
                    if let Some(input) = input {
                        self.adding.remember(&input);
                    }
                    self.monitor.remove(id);
                }
                Action::Lane(LaneAction::Move(id, to)) => self.monitor.move_host(id, to),
                Action::Lane(LaneAction::Copy(text)) | Action::Route(RouteAction::Copy(text)) => {
                    ctx.copy_text(text);
                }
                Action::Lane(LaneAction::TraceRoute(id)) => {
                    let addr = self
                        .monitor
                        .read()
                        .hosts
                        .iter()
                        .find(|h| h.id == id)
                        .and_then(|h| h.addr);
                    let Some(addr) = addr else { continue };
                    self.routes_seen = None;
                    // Asking again for the route already shown keeps its numbers.
                    let shown = self
                        .route
                        .as_ref()
                        .is_some_and(|view| view.host == id && view.trace.target() == addr);
                    if !shown {
                        let trace = self.monitor.trace(addr, TraceSettings::default());
                        self.route = Some(RouteView { host: id, trace });
                    }
                }
                Action::Lane(LaneAction::Routes(id, t0, t1)) => {
                    self.route = None;
                    self.routes_seen = Some(RouteHistory::new(id, t0, t1));
                }
                Action::Route(RouteAction::Close) => {
                    self.route = None;
                    self.routes_seen = None;
                }
                Action::Route(RouteAction::LookUpOwners(addrs)) => self.monitor.look_up_owners(addrs),
                Action::Route(RouteAction::Restart) => {
                    if let Some(view) = &self.route {
                        view.trace.restart();
                    }
                }
                Action::Route(RouteAction::Watch(addr)) => {
                    self.adding.remember(&addr);
                    add_hosts(&self.monitor, parse_targets(&addr));
                }
                Action::Lane(LaneAction::Alerts(id)) => {
                    let state = self.monitor.read();
                    if let Some(host) = state.host(id) {
                        self.host_alerts_dialog = Some(HostAlertsDialog::new(host, &self.alerts.limits));
                    }
                }
                Action::Lane(LaneAction::Rename(id)) => {
                    let state = self.monitor.read();
                    let Some(host) = state.host(id) else { continue };
                    // The host as it was added, and the address it resolved to.
                    let mut address = host.spec.host.clone();
                    if let Some(addr) = host.addr.map(|addr| addr.to_string()).filter(|addr| *addr != address) {
                        address = format!("{address}  ·  {addr}");
                    }
                    self.rename = Some(Rename {
                        what: Renaming::Host { id, address },
                        text: host.spec.label.clone().unwrap_or_default(),
                        renamed: host.spec.label.is_some(),
                        fresh: true,
                    });
                }
                Action::Lane(LaneAction::RenameNetwork(asn)) | Action::Route(RouteAction::RenameNetwork(asn)) => {
                    let state = self.monitor.read();
                    let networks = &state.networks;
                    self.rename = Some(Rename {
                        what: Renaming::Network {
                            asn,
                            registered: networks.info(asn).map(|info| info.registered.clone()),
                        },
                        text: networks.name(asn),
                        renamed: networks.user_name(asn).is_some(),
                        fresh: true,
                    });
                }
                Action::SetInterval(interval) => self.monitor.update_settings(|s| s.interval = interval),
                Action::SetNetworkLookups(on) => self.monitor.update_settings(|s| s.network_lookups = on),
                Action::TogglePause => self.monitor.update_settings(|s| s.paused = !s.paused),
                Action::ClearData => self.monitor.clear_history(),
            }
        }
    }

    /// The route panel's height, given the room between the toolbar and the status bar.
    fn route_panel_height(&self, room: f32) -> f32 {
        match self.route_height {
            Some(height) => height.clamp(ROUTE_MIN_HEIGHT, route_max_height(room)),
            // Fitting a long route by itself stops at about half, to leave the lanes room.
            None => self
                .route_fit
                .clamp(ROUTE_MIN_HEIGHT, (room * ROUTE_FIT_SHARE).max(ROUTE_MIN_HEIGHT)),
        }
    }

    /// The dialog that names a network or a host, while it's open.
    fn rename_dialog(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(rename) = &mut self.rename else {
            return;
        };
        // Some(name) sets the name; Some(None) goes back to the usual one.
        let mut chosen: Option<Option<String>> = None;
        let mut cancel = false;
        let (title, record, field_name, note, usual, usual_tip) = match &rename.what {
            Renaming::Network { asn, registered } => (
                "Name this network",
                match registered {
                    Some(registered) => format!("AS{asn}  ·  {registered}"),
                    None => format!("AS{asn}"),
                },
                "Network name",
                "Networks with the same name count as one ISP, so FerrisPing doesn't report a switch between them.",
                "Use its usual name",
                "Go back to the name FerrisPing knows it by, or the registry's",
            ),
            Renaming::Host { address, .. } => (
                "Name this host",
                address.clone(),
                "Host name",
                "Its address shows beside the name. Events already logged keep the name they have.",
                "Use its address",
                "Show this host by its address again",
            ),
        };
        let modal = egui::Modal::new(egui::Id::unique("rename")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(RichText::new(title).font(theme::font_semibold(17.0)).color(p.text));
            ui.label(RichText::new(record).size(12.5).color(p.text_muted));
            ui.add_space(6.0);
            let mut edit = TextEdit::singleline(&mut rename.text)
                .desired_width(f32::INFINITY)
                .margin(Margin::symmetric(8, 5))
                .show(ui);
            // Not while egui measures the dialog, before it's shown.
            if rename.fresh && !ui.is_sizing_pass() {
                rename.fresh = false;
                edit.response.request_focus();
                let all = egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(rename.text.chars().count()),
                );
                edit.state.cursor.set_char_range(Some(all));
                edit.state.store(ui.ctx(), edit.response.id);
            }
            let field = egui::Response::clone(&edit.response).accessible_name(field_name);
            let entered = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.add_space(2.0);
            ui.label(RichText::new(note).size(12.5).color(p.text_secondary));
            ui.add_space(10.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(accent_button(p, "Save")).clicked() || entered {
                    chosen = Some(Some(rename.text.clone()));
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                if rename.renamed && ui.button(usual).on_hover_text(usual_tip).clicked() {
                    chosen = Some(None);
                }
            });
        });
        let close = cancel || chosen.is_some() || modal.should_close();
        if let Some(name) = chosen {
            match rename.what {
                Renaming::Network { asn, .. } => self.monitor.set_network_name(asn, name),
                Renaming::Host { id, .. } => self.monitor.set_label(id, name),
            }
        }
        if close {
            self.rename = None;
        }
    }

    /// The notice about what FerrisPing sends over the network, while it's open.
    /// Only OK applies the box ticked in it.
    fn notice_dialog(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(notice) = &mut self.notice else {
            return;
        };
        let mut ok = false;
        let modal = egui::Modal::new(egui::Id::unique("network-notice")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(
                RichText::new("What FerrisPing sends over the network")
                    .font(theme::font_semibold(17.0))
                    .color(p.text),
            );
            ui.add_space(6.0);
            for text in [
                "FerrisPing pings each host you add, and looks up its name through your DNS server. An open \
                 route is traced every second, and its routers' names are looked up the same way.",
                "To show which network each host's traffic leaves through (\"via Comcast\"), the routes it \
                 took over the last day and who runs each router on a route, it can also trace the first hops \
                 to each host every 30 seconds, and ask \
                 Team Cymru's IP-to-ASN service who runs each public router it finds. Those router addresses \
                 reach your DNS server and Team Cymru.",
                "Nothing else: no telemetry, no update checks, no accounts.",
            ] {
                ui.label(RichText::new(text).color(p.text_secondary));
                ui.add_space(4.0);
            }
            ui.add_space(4.0);
            ui.checkbox(&mut notice.lookups, LOOKUPS_LABEL);
            ui.label(
                RichText::new("You can change this later under More options, at the top right.")
                    .size(12.5)
                    .color(p.text_muted),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Details:").color(p.text_secondary));
                ui.hyperlink_to("ferrisping.com/#privacy", PRIVACY_URL);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(accent_button(p, "OK")).clicked() {
                        ok = true;
                    }
                });
            });
        });
        if ok {
            let lookups = notice.lookups;
            self.notice = None;
            self.notice_answered = true;
            self.monitor.update_settings(|s| s.network_lookups = lookups);
        } else if modal.should_close() && !notice.first_run {
            // Escape or a click outside. The first run's notice ignores them: it needs an answer.
            self.notice = None;
        }
    }

    /// The Alerts dialogs, for every host and for one, while they're open.
    fn alerts_dialogs(&mut self, ctx: &egui::Context, p: &Palette, settings: &ProbeSettings) {
        if let Some(dialog) = &mut self.alerts_dialog
            && let Some(outcome) = dialog.show(ctx, p, settings.interval, settings.network_lookups)
        {
            if let Outcome::Saved(choices) = outcome {
                // The tray hears about notifications and sound on the next frame.
                self.alerts = choices;
                self.monitor.set_alerts(choices.settings());
            }
            self.alerts_dialog = None;
        }
        if let Some(dialog) = &mut self.host_alerts_dialog
            && let Some(outcome) = dialog.show(ctx, p, settings.interval, &self.alerts)
        {
            if let Outcome::Saved(alerts) = outcome {
                self.monitor.set_host_alerts(dialog.host, alerts);
            }
            self.host_alerts_dialog = None;
        }
    }

    fn take_screenshot_when_due(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.screenshot else {
            return;
        };
        if shot.requested {
            return;
        }
        let now = Instant::now();
        if now < shot.at {
            ctx.request_repaint_after(shot.at - now);
            return;
        }
        shot.requested = true;
        let path = shot.path.clone();
        let exit = shot.exit;
        let close_ctx = ctx.clone();
        ctx.request_screenshot(move |image| {
            let [width, height] = image.size;
            if let Err(error) = image::save_buffer(
                &path,
                image.as_raw(),
                width as u32,
                height as u32,
                image::ColorType::Rgba8,
            ) {
                eprintln!("Couldn't save the screenshot to {}: {error}", path.display());
            }
            if exit {
                close_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

impl eframe::App for FerrisPingApp {
    // Runs while the window is hidden in the tray too, whenever new samples or a
    // click on the tray icon ask for a repaint.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tray_logic(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let p = theme::palette(ui.visuals().dark_mode);
        let now = now_secs();
        let settings = self.monitor.settings();
        let monitor = Arc::clone(&self.monitor);
        // Held for the whole frame; changes are queued as actions and applied after.
        let state = monitor.read();
        let mut actions = Vec::new();

        self.toolbar(ui, p, &settings, &state.hosts, &mut actions);
        self.status_bar(ui, p, &state, &settings, now);
        let mut show_events = self.show_events;
        egui::Panel::right("events")
            .resizable(true)
            .default_size(300.0)
            .size_range(240.0..=(ui.available_width() - LANES_MIN_WIDTH).clamp(240.0, 520.0))
            .frame(Frame::new().fill(p.surface).inner_margin(Margin::symmetric(16, 14)))
            .show_collapsible(ui, &mut show_events, |ui| events::show(ui, &state, &self.clock, p));
        self.show_events = show_events;

        if let Some(spec) = &self.route_on_start
            && let Some(host) = state.hosts.iter().find(|h| h.spec.same_host(spec) && h.addr.is_some())
        {
            actions.push(Action::Lane(LaneAction::TraceRoute(host.id)));
            self.route_on_start = None;
        }
        let mut route_out = None;
        let panel_host =
            (self.route.as_ref().map(|view| view.host)).or(self.routes_seen.as_ref().map(|seen| seen.host));
        if let Some(id) = panel_host
            && let Some(host) = state.hosts.iter().find(|h| h.id == id)
        {
            let room = ui.available_height();
            // Lanes that don't need all the room leave it to the route, so the two
            // cards sit together (16 is the margin above the lanes).
            let under_lanes = room - 16.0 - lanes::natural_height(state.hosts.len());
            let height = self.route_panel_height(room).max(under_lanes);
            // Under the lanes, so each graph keeps the full width.
            egui::Panel::bottom("route")
                .resizable(false)
                .show_separator_line(false)
                .exact_size(height)
                .frame(Frame::new().fill(p.page).inner_margin(Margin {
                    left: 16,
                    right: 16,
                    top: 0,
                    bottom: 16,
                }))
                .show(ui, |ui| {
                    let owners = route::Owners {
                        networks: &state.networks,
                        lookups: settings.network_lookups,
                    };
                    // The traceroute, or the routes seen in a stretch of time.
                    let out = match (&self.route, &mut self.routes_seen) {
                        (Some(view), _) => route::show(ui, view, host, owners, &self.clock, p),
                        (None, Some(seen)) => route_history::show(ui, seen, host, owners, &self.clock, p),
                        (None, None) => RouteOutput::default(),
                    };
                    route_out = Some((room, height, out));
                });
        }
        let route_open = route_out.is_some();
        if let Some((room, height, out)) = route_out {
            self.route_fit = height + out.missing_height;
            if out.fit {
                self.route_height = None;
            } else if out.resized_by != 0.0 {
                self.route_height = Some((height + out.resized_by).clamp(ROUTE_MIN_HEIGHT, route_max_height(room)));
            }
            if (self.route_panel_height(room) - height).abs() > 0.5 {
                ui.ctx().request_repaint();
            }
            actions.extend(out.actions.into_iter().map(Action::Route));
        }

        egui::CentralPanel::default()
            .frame(Frame::new().fill(p.page).inner_margin(Margin {
                // The route panel brings its own gap, which is also its resize handle.
                bottom: if route_open { 0 } else { 16 },
                ..Margin::same(16)
            }))
            .show(ui, |ui| {
                if state.hosts.is_empty() {
                    self.empty_state(ui, p, &mut actions);
                    return;
                }
                let view = self.view(now, &settings);
                let out = lanes::show(ui, &state, &view, &self.clock, p);
                if out.left_live {
                    self.live = false;
                }
                if out.go_live {
                    self.live = true;
                }
                self.bounds = out.bounds;
                actions.extend(out.actions.into_iter().map(Action::Lane));
            });
        drop(state);

        let ctx = ui.ctx().clone();
        self.rename_dialog(&ctx, p);
        self.notice_dialog(&ctx, p);
        self.alerts_dialogs(&ctx, p, &settings);
        self.apply(&ctx, actions);
        let redraw = if self.hidden {
            HIDDEN_REDRAW
        } else {
            redraw_interval(self.window_secs)
        };
        self.redraw_ms.store(redraw.as_millis() as u64, Ordering::Relaxed);
        if self.live && !settings.paused && !self.hidden {
            // Keep the graphs sliding toward the newest samples.
            ctx.request_repaint_after(redraw);
        }
        self.take_screenshot_when_due(&ctx);
    }

    fn persist_egui_memory(&self) -> bool {
        self.screenshot.is_none()
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.session {
            return;
        }
        // One lock for everything: a second one taken while the first is held can
        // deadlock against a probe waiting to write.
        let state = self.monitor.read();
        let settings = self.monitor.settings();
        let saved = Saved {
            hosts: state.hosts.iter().map(|h| h.spec.to_input()).collect(),
            interval_ms: settings.interval.as_millis() as u64,
            window_secs: self.window_secs,
            theme: self.theme,
            show_events: self.show_events,
            show_jitter: self.show_jitter,
            network_names: state
                .networks
                .user_names()
                .map(|(asn, name)| (asn, name.to_string()))
                .collect(),
            // Until the first run's notice is answered, lookups are off whatever's ticked
            // in it, so keep what's ticked for next time.
            network_lookups: match &self.notice {
                Some(notice) if notice.first_run => notice.lookups,
                _ => settings.network_lookups,
            },
            notice_answered: self.notice_answered,
            alerts: self.alerts,
            host_alerts: state
                .hosts
                .iter()
                .filter_map(|h| HostChoice::of(h.alerts, &self.alerts.limits).map(|choice| (h.spec.to_input(), choice)))
                .collect(),
            close_to_tray: self.close_to_tray,
            tray_tip_shown: self.tray_tip_shown,
            recent_hosts: self.adding.recent.clone(),
        };
        drop(state);
        eframe::set_value(storage, STORAGE_KEY, &saved);
    }

    fn on_exit(&mut self) {
        // Take the icon away now, rather than leave it for Windows to notice.
        self.tray = None;
    }
}

fn route_max_height(room: f32) -> f32 {
    (room - LANES_MIN_HEIGHT).max(ROUTE_MIN_HEIGHT)
}

/// Watch these hosts, skipping any already watched or listed twice.
fn add_hosts(monitor: &Monitor, specs: Vec<TargetSpec>) {
    let mut watched: Vec<TargetSpec> = monitor.read().hosts.iter().map(|h| h.spec.clone()).collect();
    for spec in specs {
        if !watched.iter().any(|w| w.same_host(&spec)) {
            watched.push(spec.clone());
            monitor.add(spec);
        }
    }
}

/// How often to redraw a live view: about when the graph moves by one point
/// (a graph is roughly 1000 points wide), at most 10 and at least 1 time a second.
fn redraw_interval(window_secs: u64) -> Duration {
    Duration::from_secs_f64((window_secs as f64 / 1000.0).clamp(0.1, 1.0))
}

/// How often to redraw while hidden in the tray, for its tooltip. egui on Windows can't
/// tell a hidden window from a shown one, so it would go on drawing graphs nobody sees.
const HIDDEN_REDRAW: Duration = Duration::from_secs(1);

/// `6 hosts · 5 up · 1 down · every 1 s · 2 s timeout`
fn summary(state: &State, settings: &ProbeSettings) -> String {
    let count = |status| state.hosts.iter().filter(|h| h.status == status).count();
    let hosts = state.hosts.len();
    let mut parts = vec![format!("{hosts} host{}", if hosts == 1 { "" } else { "s" })];
    let (up, down) = (count(HostStatus::Up), count(HostStatus::Down));
    if hosts > 0 {
        parts.push(format!("{up} up"));
    }
    if down > 0 {
        parts.push(format!("{down} down"));
    }
    if settings.paused {
        parts.push("paused".into());
    } else {
        parts.push(format!("every {}", format::interval(settings.interval)));
    }
    parts.push(format!("{} timeout", format::interval(settings.timeout)));
    parts.join("  ·  ")
}

pub fn accent_button<'a>(p: &Palette, label: &str) -> Button<'a> {
    Button::new(RichText::new(label).color(p.on_accent))
        .fill(p.accent)
        .stroke(Stroke::NONE)
}

fn live_badge(ui: &mut Ui, p: &Palette, paused: bool) {
    let (text, dot) = if paused {
        ("Paused", p.text_muted)
    } else {
        ("Live", p.accent)
    };
    ui.label(RichText::new(text).color(p.text_secondary));
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, dot);
}

/// Too many probes are out for the interval, and some are being skipped.
fn skipping_badge(ui: &mut Ui, p: &Palette) {
    ui.label(RichText::new("Skipping probes").size(12.5).color(p.text_secondary))
        .on_hover_text(
            "Too many probes are waiting on hosts that don't answer, so some of \
             theirs are skipped. Hosts that answer keep their pace. A longer \
             interval or fewer hosts fixes it.",
        );
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, p.warning);
}

fn interval_picker(ui: &mut Ui, settings: &ProbeSettings, actions: &mut Vec<Action>) {
    let current = settings.interval.as_millis() as u64;
    let mut chosen = current;
    ComboBox::from_id_salt("interval")
        .selected_text(format!("Every {}", format::interval(settings.interval)))
        .width(112.0)
        .show_ui(ui, |ui| {
            for ms in INTERVALS_MS {
                ui.selectable_value(&mut chosen, ms, format::interval(Duration::from_millis(ms)));
            }
        })
        .response
        .on_hover_text("How often each host is pinged")
        .accessible_name("Ping interval");
    if chosen != current {
        actions.push(Action::SetInterval(Duration::from_millis(chosen)));
    }
}

/// Widths the toolbar measured in earlier frames, to fit a narrow window. Zero until
/// measured.
#[derive(Clone, Copy, Debug, Default)]
struct ToolbarWidths {
    /// The controls on the right, with the window spans as buttons, at their widest so
    /// far: "Back to live" and "Resume" take more room than what they replace.
    controls: f32,
    /// The Add button, and the ⌄ button beside the box.
    add: f32,
    more: f32,
}

/// What the toolbar shows in a window of its width.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ToolbarFit {
    add: add::Form,
    /// The window spans as a menu rather than a row of buttons.
    spans_menu: bool,
}

impl ToolbarWidths {
    /// The most the toolbar can show in `room`, what's left after the logo and name,
    /// with `gap` between controls. The add box folds into its button first, and then
    /// the window spans into a menu.
    fn fit(&self, room: f32, gap: f32) -> ToolbarFit {
        let box_room = room - self.controls - (self.more + self.add + 3.0 * gap);
        if box_room >= add::BOX_MIN {
            ToolbarFit {
                add: add::Form::Box(box_room.min(add::BOX_MAX)),
                spans_menu: false,
            }
        } else {
            ToolbarFit {
                add: add::Form::Button,
                spans_menu: self.add + gap + self.controls > room,
            }
        }
    }

    /// Whether these are the widths `other` measured, give or take rounding.
    fn same_as(&self, other: &Self) -> bool {
        let close = |a: f32, b: f32| (a - b).abs() < 0.5;
        close(self.controls, other.controls) && close(self.add, other.add) && close(self.more, other.more)
    }
}

/// The window spans as a menu, for a toolbar too narrow for the row of buttons. Returns
/// true when the value changed.
fn spans_menu(ui: &mut Ui, value: &mut u64, options: &[(u64, &str)]) -> bool {
    let before = *value;
    let current = options
        .iter()
        .find(|(option, _)| *option == *value)
        .map_or_else(|| format::duration(*value as f64), |(_, label)| label.to_string());
    ComboBox::from_id_salt("window")
        .selected_text(current)
        .width(56.0)
        .show_ui(ui, |ui| {
            for &(option, label) in options {
                ui.selectable_value(value, option, label);
            }
        })
        .response
        .on_hover_text("How much time the graphs show")
        .accessible_name("Time shown");
    *value != before
}

/// A row of mutually exclusive buttons. Returns true when the value changed.
fn segmented(ui: &mut Ui, p: &Palette, value: &mut u64, options: &[(u64, &str)]) -> bool {
    let mut changed = false;
    Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            // In a right-to-left parent the buttons are placed in reverse.
            let ordered: Vec<&(u64, &str)> = if ui.layout().prefer_right_to_left() {
                options.iter().rev().collect()
            } else {
                options.iter().collect()
            };
            for (option, label) in ordered {
                let selected = *value == *option;
                let text = RichText::new(*label).color(if selected { p.text } else { p.text_secondary });
                let button = if selected {
                    Button::new(text)
                        .fill(p.surface)
                        .stroke(Stroke::new(1.0, p.border_strong))
                } else {
                    Button::new(text).frame_when_inactive(false)
                };
                let response = ui.add(button.corner_radius(CornerRadius::same(6)).min_size(vec2(38.0, 24.0)));
                if response.clicked() && !selected {
                    *value = *option;
                    changed = true;
                }
            }
        });
    changed
}
