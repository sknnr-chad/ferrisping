//! The routes a host's traffic took during a stretch of time picked on its graph
//! with Shift+drag: side by side, each with when it was in use, the hops where it
//! differs from the route before, and how long each hop took against the usual on
//! that route. It takes the route panel's place, and comes from the route checks
//! every host gets (see [`Paths`](ferrisping_core::Paths)).

use std::fmt::Write as _;
use std::net::IpAddr;

use eframe::egui::text::LayoutJob;
use eframe::egui::{
    self, Align, Button, CornerRadius, Frame, Label, Layout, Margin, RichText, Sense, Stroke, TextFormat, Ui, vec2,
};
use egui_extras::{Column, TableBuilder};
use ferrisping_core::{EXIT_CHECK_EVERY, EXIT_DOWN_CHECK_EVERY, Host, HostId, Owner, PathSpan, RouterSeen};

use crate::format::{self, Clock};
use crate::route::{self, Owners, RouteAction, RouteOutput};
use crate::theme::{self, Palette};

const HOP_WIDTH: f32 = 40.0;
const ROUTE_WIDTH: f32 = 250.0;
const HEADER_HEIGHT: f32 = 64.0;
const ROW_HEIGHT: f32 = 42.0;
/// A hop took longer than usual when it took this much longer, and this share of
/// the usual, at least: less is the noise of one check.
const SLOWER_MS: f32 = 5.0;
const SLOWER_SHARE: f32 = 0.25;
/// A switch to another network marks a change of route this close to it already.
const NEAR_SWITCH_SECS: f64 = 90.0;
/// How far out the route checks go.
const MAX_HOPS: usize = 12;

/// What the panel shows: a host, and the stretch of time picked on its graph.
pub struct RouteHistory {
    pub host: HostId,
    pub t0: f64,
    pub t1: f64,
    /// The host's paths as they were when their routers' owners were last asked
    /// for, so that's asked once per change.
    asked: Option<u64>,
}

impl RouteHistory {
    pub fn new(host: HostId, t0: f64, t1: f64) -> Self {
        Self {
            host,
            t0: t0.min(t1),
            t1: t0.max(t1),
            asked: None,
        }
    }
}

/// A change of route, to mark on the host's graph.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteChange {
    pub at: f64,
    /// The hops, counted from 1, where the new route differs from the one before.
    pub hops: Vec<u8>,
}

/// The host's changes of route, oldest first. Those that came with a switch to
/// another network are left out: the switch marks them.
pub fn changes(host: &Host) -> Vec<RouteChange> {
    host.paths
        .changes()
        .filter(|path| {
            !host
                .exit_changes
                .iter()
                .any(|switch| (switch.at - path.first).abs() <= NEAR_SWITCH_SECS)
        })
        .map(|path| RouteChange {
            at: path.first,
            hops: path.changed.clone(),
        })
        .collect()
}

/// A change of route, for the graph's hover text.
pub fn change_note(change: &RouteChange) -> String {
    let from = change.hops.first().copied().unwrap_or(1);
    format!("New route from hop {from}. Shift+drag across it to compare.")
}

pub fn show(
    ui: &mut Ui,
    view: &mut RouteHistory,
    host: &Host,
    owners: Owners<'_>,
    clock: &Clock,
    p: &Palette,
) -> RouteOutput {
    let spans = host.paths.during(view.t0, view.t1);
    let labels = labels(&spans);
    let mut out = RouteOutput::default();
    // Who runs each router: looked up as the route checks do, while lookups are on.
    if owners.lookups && view.asked != Some(host.paths.version()) {
        view.asked = Some(host.paths.version());
        out.actions.push(RouteAction::LookUpOwners(routers_in(&spans)));
    }

    let handle = route::resize_handle(ui, p);
    out.resized_by = -handle.drag_delta().y;
    out.fit = handle.double_clicked();
    let notes = notes(view, host, &spans, &labels, owners, clock);

    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin {
            left: 16,
            right: 12,
            top: 10,
            bottom: 10,
        })
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            header(ui, view, host, &spans, &labels, owners, clock, p, &mut out.actions);
            ui.add_space(4.0);
            // The notes go in first, along the bottom, so the table gets exactly the
            // space left and scrolls inside it.
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                if !notes.is_empty() {
                    ui.add(Label::new(RichText::new(notes.join("\n")).size(12.0).color(p.text_muted)).wrap());
                    ui.add_space(4.0);
                }
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    out.missing_height = if spans.is_empty() {
                        empty(ui, owners, p)
                    } else {
                        table(ui, view, host, &spans, &labels, owners, clock, p, &mut out.actions)
                    };
                });
            });
        });
    out
}

/// Each route's name: a letter, the same one for a route seen again.
fn labels(spans: &[PathSpan<'_>]) -> Vec<String> {
    let mut labels: Vec<String> = Vec::with_capacity(spans.len());
    let mut routes = 0;
    for (i, span) in spans.iter().enumerate() {
        let seen = spans[..i]
            .iter()
            .position(|earlier| earlier.path.same_route(span.path))
            .map(|j| labels[j].clone());
        labels.push(seen.unwrap_or_else(|| {
            routes += 1;
            match u8::try_from(routes - 1).ok().filter(|&n| n < 26) {
                Some(n) => char::from(b'A' + n).to_string(),
                None => routes.to_string(),
            }
        }));
    }
    labels
}

/// Every router the routes went through.
fn routers_in(spans: &[PathSpan<'_>]) -> Vec<IpAddr> {
    let mut addrs: Vec<IpAddr> = Vec::new();
    for span in spans {
        for hop in &span.path.hops {
            for router in &hop.routers {
                if !addrs.contains(&router.addr) {
                    addrs.push(router.addr);
                }
            }
        }
    }
    addrs
}

#[allow(clippy::too_many_arguments)]
fn header(
    ui: &mut Ui,
    view: &RouteHistory,
    host: &Host,
    spans: &[PathSpan<'_>],
    labels: &[String],
    owners: Owners<'_>,
    clock: &Clock,
    p: &Palette,
    actions: &mut Vec<RouteAction>,
) {
    egui::Sides::new().height(30.0).shrink_left().truncate().show(
        ui,
        |ui| {
            ui.label(
                RichText::new("Routes to")
                    .font(theme::font(15.0))
                    .color(p.text_secondary),
            );
            ui.label(
                RichText::new(host.name())
                    .font(theme::font_semibold(15.0))
                    .color(p.text),
            );
            let summary = format!(
                "{} – {}  ·  {}",
                clock.day_time(view.t0),
                clock.day_time(view.t1),
                summary(view, spans)
            );
            ui.add(Label::new(RichText::new(summary).color(p.text_muted)).truncate());
        },
        |ui| {
            let close = Button::new(
                RichText::new(theme::icon(theme::ICON_CLOSE, "×"))
                    .size(11.0)
                    .color(p.text_muted),
            )
            .frame_when_inactive(false)
            .min_size(vec2(26.0, 24.0));
            if ui
                .add(close)
                .on_hover_text("Close")
                .accessible_name("Close routes")
                .clicked()
            {
                actions.push(RouteAction::Close);
            }
            if !spans.is_empty()
                && ui
                    .button("Copy report")
                    .on_hover_text("Copy these routes as text, e.g. for a ticket")
                    .clicked()
            {
                actions.push(RouteAction::Copy(report(view, host, spans, labels, owners, clock)));
            }
        },
    );
}

/// "one route", "the route changed twice" and the like.
fn summary(view: &RouteHistory, spans: &[PathSpan<'_>]) -> String {
    let changes = spans.iter().filter(|span| span.began_within(view.t0, view.t1)).count();
    match (spans.len(), changes) {
        (0, _) => "no route checks".to_string(),
        (_, 0) => "one route".to_string(),
        (_, 1) => "the route changed once".to_string(),
        (_, 2) => "the route changed twice".to_string(),
        (_, n) => format!("the route changed {n} times"),
    }
}

/// Why there's nothing to show. Returns how much room is to spare, as a negative
/// missing height.
fn empty(ui: &mut Ui, owners: Owners<'_>, p: &Palette) -> f32 {
    let text = if owners.lookups {
        format!(
            "No route checks in this stretch of time. Each host's route is checked every {}, or every {} \
             while it's down after answering, and kept for a day.",
            format::interval(EXIT_CHECK_EVERY),
            format::interval(EXIT_DOWN_CHECK_EVERY)
        )
    } else {
        "Routes are only checked while network lookups are on. Turn them on under More options, \
         at the top right."
            .to_string()
    };
    ui.add_space(6.0);
    ui.add(Label::new(RichText::new(text).color(p.text_muted)).wrap());
    -ui.available_height()
}

/// Draws the routes side by side and returns how much taller they'd need to be to
/// show every hop.
#[allow(clippy::too_many_arguments)]
fn table(
    ui: &mut Ui,
    view: &RouteHistory,
    host: &Host,
    spans: &[PathSpan<'_>],
    labels: &[String],
    owners: Owners<'_>,
    clock: &Clock,
    p: &Palette,
    actions: &mut Vec<RouteAction>,
) -> f32 {
    let rows = spans.iter().map(|span| span.path.shown_hops().len()).max().unwrap_or(0);
    let right = Layout::right_to_left(Align::Center);
    let mut missing = 0.0;
    egui::ScrollArea::horizontal()
        .id_salt("route-history-across")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let body = TableBuilder::new(ui)
                .id_salt("route-history")
                .cell_layout(Layout::top_down(Align::Min).with_main_align(Align::Center))
                .column(Column::exact(HOP_WIDTH))
                .columns(Column::initial(ROUTE_WIDTH).at_least(160.0).clip(true), spans.len())
                .auto_shrink([false, false])
                .min_scrolled_height(0.0)
                .header(HEADER_HEIGHT, |mut header| {
                    header.col(|ui| {
                        ui.with_layout(right, |ui| {
                            ui.label(RichText::new("Hop").size(11.5).color(p.text_muted))
                        });
                    });
                    for (span, label) in spans.iter().zip(labels) {
                        header.col(|ui| inset(ui, |ui| column_head(ui, view, span, label, clock, p)));
                    }
                })
                .body(|body| {
                    body.rows(ROW_HEIGHT, rows, |mut row| {
                        let k = row.index();
                        row.col(|ui| {
                            ui.with_layout(right, |ui| {
                                ui.label(RichText::new((k + 1).to_string()).color(p.text_muted))
                            });
                        });
                        for (i, span) in spans.iter().enumerate() {
                            // Against the route before, when that's the column on the left
                            // or the change came in this stretch.
                            let changed = (i > 0 || span.began_within(view.t0, view.t1))
                                && span.path.changed.contains(&(k as u8 + 1));
                            row.col(|ui| {
                                if changed {
                                    let rect = ui.max_rect().shrink2(vec2(0.0, 1.0));
                                    let fill = p.accent.gamma_multiply(0.16);
                                    ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
                                }
                                inset(ui, |ui| hop_cell(ui, host, span, k, changed, owners, p, actions));
                            });
                        }
                    });
                });
            missing = body.content_size.y - body.inner_rect.height();
        });
    missing
}

/// A route column's cell, set in from its edges so a highlight shows around the text.
fn inset(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui)) {
    let margin = Margin {
        left: 6,
        right: 4,
        top: 0,
        bottom: 0,
    };
    Frame::new().inner_margin(margin).show(ui, add_contents);
}

/// A route's name, when it was in use and how many checks saw it.
fn column_head(ui: &mut Ui, view: &RouteHistory, span: &PathSpan<'_>, label: &str, clock: &Clock, p: &Palette) {
    ui.spacing_mut().item_spacing.y = 1.0;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("Route {label}"))
                .font(theme::font_semibold(13.5))
                .color(p.text),
        );
        if span.began_within(view.t0, view.t1) {
            ui.label(RichText::new(format!("new at {}", clock.time(span.path.first))).color(p.accent))
                .on_hover_text(format!(
                    "The route changed: {} differ from the route before.",
                    hop_list(&span.path.changed)
                ));
        }
    });
    match (span.first, span.last) {
        (Some(first), Some(last)) => {
            let when = if last - first < 1.0 {
                clock.time(first)
            } else {
                format!("{} – {}", clock.time(first), clock.time(last))
            };
            ui.label(RichText::new(when).size(12.0).color(p.text_muted));
            let color = match span.reached {
                0 => p.critical,
                reached if reached < span.checks => p.serious,
                _ => p.text_muted,
            };
            ui.label(
                RichText::new(format!(
                    "{}, the host answered {}",
                    count(span.checks, "check", "checks"),
                    span.reached
                ))
                .size(12.0)
                .color(color),
            );
        }
        _ => {
            ui.label(RichText::new("In use as this began").size(12.0).color(p.text_muted));
            ui.label(
                RichText::new(format!("last checked {}", clock.time(span.path.last)))
                    .size(12.0)
                    .color(p.text_muted),
            );
        }
    }
}

/// Who answered at one hop of a route, and how fast.
struct Cell<'a> {
    routers: &'a [RouterSeen],
    host: u32,
    ms: Option<f32>,
    usual_ms: Option<f32>,
}

/// Hop `k` (from 0) of a span's route: as its checks saw it, or for a route in use
/// as the stretch began, as it was.
fn cell<'a>(span: &'a PathSpan<'_>, k: usize) -> Option<Cell<'a>> {
    if span.checks > 0 {
        let hop = span.hops.get(k)?;
        Some(Cell {
            routers: &hop.routers,
            host: hop.host,
            ms: hop.ms,
            usual_ms: hop.usual_ms,
        })
    } else {
        let hop = span.path.hops.get(k)?;
        Some(Cell {
            routers: &hop.routers,
            host: hop.host,
            ms: None,
            usual_ms: None,
        })
    }
}

/// How much longer a hop took than usual, when that's more than the noise.
fn slower_by(ms: Option<f32>, usual_ms: Option<f32>) -> Option<f32> {
    let (ms, usual) = (ms?, usual_ms?);
    let extra = ms - usual;
    (extra >= SLOWER_MS.max(usual * SLOWER_SHARE)).then_some(extra)
}

#[allow(clippy::too_many_arguments)]
fn hop_cell(
    ui: &mut Ui,
    host: &Host,
    span: &PathSpan<'_>,
    k: usize,
    changed: bool,
    owners: Owners<'_>,
    p: &Palette,
    actions: &mut Vec<RouteAction>,
) {
    let Some(cell) = cell(span, k) else {
        return;
    };
    ui.spacing_mut().item_spacing.y = 1.0;
    let changed_note = changed.then_some("Different from the route before.");
    match cell.routers.first() {
        Some(first) => {
            let font = egui::TextStyle::Body.resolve(ui.style());
            let mut job = LayoutJob::default();
            let mut part = |text: &str, color| job.append(text, 0.0, TextFormat::simple(font.clone(), color));
            part(&first.addr.to_string(), p.text);
            if cell.routers.len() > 1 {
                part(&format!("  +{}", cell.routers.len() - 1), p.accent);
            }
            if cell.host > 0 {
                part("  or the host", p.text_muted);
            }
            let response = ui.add(Label::new(job).truncate().sense(Sense::click()));
            let response = response.on_hover_ui(|ui| {
                for router in cell.routers {
                    ui.label(format!(
                        "{}  ({})",
                        router.addr,
                        count(router.checks, "check", "checks")
                    ));
                }
                if cell.routers.len() > 1 {
                    ui.label(
                        RichText::new("Several routers took turns here: the traffic is load-balanced.")
                            .color(p.text_muted),
                    );
                }
                if let Some(note) = changed_note {
                    ui.label(RichText::new(note).color(p.accent));
                }
                ui.label(RichText::new("Right-click to watch or copy this router.").color(p.text_muted));
            });
            let ip = first.addr.to_string();
            response.context_menu(|ui| {
                if ui
                    .button(format!("Watch {ip}"))
                    .on_hover_text("Ping this router in a lane of its own")
                    .clicked()
                {
                    actions.push(RouteAction::Watch(ip.clone()));
                    ui.close();
                }
                if ui.button(format!("Copy {ip}")).clicked() {
                    actions.push(RouteAction::Copy(ip.clone()));
                    ui.close();
                }
            });
        }
        None if cell.host > 0 => {
            let response = ui.add(Label::new(RichText::new(host.name()).color(p.text)).truncate());
            if let Some(note) = changed_note {
                response.on_hover_text(note);
            }
        }
        None => {
            let response = ui.label(RichText::new("No reply").color(p.text_muted));
            response.on_hover_text(match changed_note {
                Some(note) => note.to_string(),
                None => "Nothing answered at this hop. Plenty of routers never answer route \
                         checks, which is harmless when the hops after them do."
                    .to_string(),
            });
        }
    }
    if cell.ms.is_none() && cell.routers.is_empty() {
        return;
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if let Some(ms) = cell.ms {
            let usual = cell
                .usual_ms
                .map(|usual| format!(" It's usually {} on this route.", format::ms_unit(usual)))
                .unwrap_or_default();
            ui.label(RichText::new(format::ms_unit(ms)).color(p.text_secondary))
                .on_hover_text(format!("The middle round trip over these checks.{usual}"));
        }
        if let Some(extra) = slower_by(cell.ms, cell.usual_ms) {
            ui.label(RichText::new(format!("+{}", format::ms(extra))).color(p.serious))
                .on_hover_text(format!("{} longer than usual on this route", format::ms_unit(extra)));
        }
        if let Some(first) = cell.routers.first() {
            network(ui, owners, first.addr, p, actions);
        }
    });
}

/// The network a router belongs to, when that's known.
fn network(ui: &mut Ui, owners: Owners<'_>, addr: IpAddr, p: &Palette, actions: &mut Vec<RouteAction>) {
    let text = match owners.of(addr) {
        Some(Owner::Network(asn)) => return route::network_label(ui, owners.networks, asn, p, actions),
        Some(Owner::Private) => "private",
        Some(Owner::Unannounced) => "not announced",
        None => return,
    };
    ui.add(Label::new(RichText::new(text).color(p.text_muted)).truncate());
}

/// What the table doesn't say by itself.
fn notes(
    view: &RouteHistory,
    host: &Host,
    spans: &[PathSpan<'_>],
    labels: &[String],
    owners: Owners<'_>,
    clock: &Clock,
) -> Vec<String> {
    let mut notes = Vec::new();
    for (i, (span, label)) in spans.iter().zip(labels).enumerate() {
        if span.checks == 0 {
            continue;
        }
        // A new route held against the one before: where its extra delay starts.
        if let Some(before) = i.checked_sub(1).map(|j| &spans[j])
            && let Some(k) =
                delay_from((span.hops.iter().zip(&before.hops)).map(|(hop, then)| (hop.ms, then.ms.or(then.usual_ms))))
        {
            notes.push(format!(
                "Round trips on route {label} were longer than on route {} from hop {} on: that's where \
                 the extra delay starts.",
                labels[i - 1],
                k + 1
            ));
        }
        if span.reached == 0 {
            // Where the way stopped, during an outage.
            let last = span.hops.iter().rposition(|hop| !hop.routers.is_empty());
            notes.push(match last {
                Some(k) => format!(
                    "On route {label} the host didn't answer, and nothing answered past hop {} ({}).",
                    k + 1,
                    span.hops[k].routers[0].addr
                ),
                None => format!("On route {label} nothing answered, not even the first hop."),
            });
        }
        if let Some(k) = delay_from(span.hops.iter().map(|hop| (hop.ms, hop.usual_ms))) {
            notes.push(format!(
                "On route {label} round trips were up from hop {} on: that's where the extra delay starts.",
                k + 1
            ));
        }
    }
    if let Some(since) = host.paths.unsettled_since()
        && since <= view.t1
    {
        notes.push(format!(
            "Checks since {} found a router new to the route. The next few tell whether the route changed.",
            clock.time(since)
        ));
    }
    if !spans.is_empty() && !owners.lookups {
        notes.push("Network lookups are off, so routes aren't being checked now.".to_string());
    }
    if spans
        .iter()
        .any(|span| !span.path.reaches_host() && span.path.hops.len() >= MAX_HOPS)
    {
        notes.push(format!(
            "Route checks go {MAX_HOPS} hops out, and the host is further than that."
        ));
    }
    if spans
        .iter()
        .any(|span| span.hops.iter().any(|hop| slower_by(hop.ms, hop.usual_ms).is_some()))
    {
        notes.push("Red figures: how much longer than usual a hop took on its route.".to_string());
    }
    notes
}

/// Given each hop's round trip and what to hold it against, the first hop from which
/// round trips were up on that at every hop with both: where extra delay starts. A
/// router slow to answer while the hops after it aren't is only slow at answering.
fn delay_from(hops: impl IntoIterator<Item = (Option<f32>, Option<f32>)>) -> Option<usize> {
    let mut start = None;
    for (k, (ms, against)) in hops.into_iter().enumerate() {
        if ms.is_none() || against.is_none() {
            continue;
        }
        if slower_by(ms, against).is_some() {
            start.get_or_insert(k);
        } else {
            start = None;
        }
    }
    start
}

/// "hop 3", "hops 3–4", "hops 2–5 and 7".
fn hop_list(hops: &[u8]) -> String {
    let mut runs: Vec<(u8, u8)> = Vec::new();
    for &hop in hops {
        match runs.last_mut() {
            Some((_, last)) if *last + 1 == hop => *last = hop,
            _ => runs.push((hop, hop)),
        }
    }
    let parts: Vec<String> = runs
        .iter()
        .map(|&(first, last)| match last - first {
            0 => first.to_string(),
            _ => format!("{first}–{last}"),
        })
        .collect();
    let word = if hops.len() == 1 { "hop" } else { "hops" };
    match parts.split_last() {
        None => "no hops".to_string(),
        Some((last, [])) => format!("{word} {last}"),
        Some((last, rest)) => format!("{word} {} and {last}", rest.join(", ")),
    }
}

/// "1 check", "3 checks".
fn count(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The routes as plain text, for pasting into a ticket or a chat.
fn report(
    view: &RouteHistory,
    host: &Host,
    spans: &[PathSpan<'_>],
    labels: &[String],
    owners: Owners<'_>,
    clock: &Clock,
) -> String {
    let mut out = String::new();
    let address = host.address();
    let who = if address == host.name() {
        address
    } else {
        format!("{} ({address})", host.name())
    };
    let _ = writeln!(
        out,
        "FerrisPing routes to {who} from {} to {}: {}",
        clock.day_time(view.t0),
        clock.day_time(view.t1),
        summary(view, spans)
    );
    let ms = |value: Option<f32>| value.map_or_else(|| "-".to_string(), format::ms_unit);
    for (span, label) in spans.iter().zip(labels) {
        let _ = writeln!(out);
        let mut title = format!("Route {label}");
        if span.began_within(view.t0, view.t1) {
            let _ = write!(
                title,
                ", new at {} ({} changed)",
                clock.time(span.path.first),
                hop_list(&span.path.changed)
            );
        }
        match (span.first, span.last) {
            (Some(first), Some(last)) => {
                let _ = write!(
                    title,
                    ": {} to {}, {}, the host answered {}",
                    clock.time(first),
                    clock.time(last),
                    count(span.checks, "check", "checks"),
                    span.reached
                );
            }
            _ => {
                let _ = write!(
                    title,
                    ": in use as this began, last checked {}",
                    clock.time(span.path.last)
                );
            }
        }
        let _ = writeln!(out, "{title}");
        let _ = writeln!(
            out,
            "{:>5}  {:<44} {:<24} {:>9} {:>9}",
            "Hop", "Router", "Network", "Middle", "Usual"
        );
        for k in 0..span.path.shown_hops().len() {
            let Some(cell) = cell(span, k) else { continue };
            let mut who = match cell.routers.first() {
                Some(first) => first.addr.to_string(),
                None if cell.host > 0 => host.name().to_string(),
                None => "*".to_string(),
            };
            if cell.routers.len() > 1 {
                let _ = write!(who, " +{}", cell.routers.len() - 1);
            }
            let network = match cell.routers.first().and_then(|first| owners.of(first.addr)) {
                Some(Owner::Network(asn)) => format!("{} AS{asn}", owners.networks.name(asn)),
                Some(Owner::Private) => "private".to_string(),
                Some(Owner::Unannounced) => "not announced".to_string(),
                None => String::new(),
            };
            let changed = span.path.changed.contains(&(k as u8 + 1)) && span.began_within(view.t0, view.t1);
            let _ = writeln!(
                out,
                "{:>5}  {:<44} {:<24} {:>9} {:>9}{}",
                k + 1,
                who,
                network,
                ms(cell.ms),
                ms(cell.usual_ms),
                if changed { "  changed" } else { "" }
            );
        }
    }
    let notes = notes(view, host, spans, labels, owners, clock);
    if !notes.is_empty() {
        let _ = writeln!(out);
        for note in notes {
            let _ = writeln!(out, "{note}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_lists_read_as_words() {
        assert_eq!(hop_list(&[3]), "hop 3");
        assert_eq!(hop_list(&[3, 4]), "hops 3–4");
        assert_eq!(hop_list(&[2, 3, 4, 5, 7]), "hops 2–5 and 7");
        assert_eq!(hop_list(&[1, 4, 6, 7]), "hops 1, 4 and 6–7");
    }

    #[test]
    fn extra_delay_starts_where_every_hop_after_is_slower() {
        let hops = |pairs: &[(f32, f32)]| -> Vec<(Option<f32>, Option<f32>)> {
            pairs.iter().map(|&(ms, against)| (Some(ms), Some(against))).collect()
        };
        assert_eq!(delay_from(hops(&[(1.0, 1.0), (40.0, 8.0), (44.0, 12.0)])), Some(1));
        // A router slow to answer, with the hops after it as fast as ever.
        assert_eq!(delay_from(hops(&[(1.0, 1.0), (40.0, 8.0), (12.0, 12.0)])), None);
        // A hop without a round trip to compare says nothing either way.
        let mut gap = hops(&[(1.0, 1.0), (40.0, 8.0), (44.0, 12.0), (50.0, 15.0)]);
        gap[2].1 = None;
        assert_eq!(delay_from(gap), Some(1));
    }

    #[test]
    fn only_a_clear_slowdown_counts() {
        assert_eq!(slower_by(Some(80.0), Some(30.0)), Some(50.0));
        // Under 5 ms, or a quarter of the usual, is noise.
        assert_eq!(slower_by(Some(13.0), Some(10.0)), None);
        assert_eq!(slower_by(Some(240.0), Some(200.0)), None);
        assert_eq!(slower_by(Some(80.0), None), None);
    }
}
