//! The route panel: a traceroute to one host that keeps running, so loss and
//! delay can be pinned on a hop. Opened from a lane. Each hop shows the network
//! (ISP) it belongs to, so the panel also shows where traffic crosses between them.

use std::fmt::Write as _;
use std::net::IpAddr;

use eframe::egui::text::LayoutJob;
use eframe::egui::{
    self, Align, Button, CornerRadius, CursorIcon, Frame, Label, Layout, Margin, Rect, Response, RichText, Sense,
    Stroke, TextFormat, Ui, pos2, vec2,
};
use egui_extras::{Column, TableBuilder, TableRow};
use ferrisping_core::{Hop, Host, HostId, Networks, Owner, RECENT_RESULTS, Route, Trace};

use crate::format::{self, Clock};
use crate::series;
use crate::theme::{self, Palette};

/// Room between the lanes and the panel, which is also the handle that resizes it.
const GAP: f32 = 16.0;
const ROW_HEIGHT: f32 = 28.0;
const HEADER_HEIGHT: f32 = 22.0;
/// Percentage points a hop may lose beyond the hops after it before the panel
/// calls out the difference as the router limiting its replies.
const RATE_LIMIT_MARGIN: f32 = 1.0;
const NETWORK_WIDTH: f32 = 110.0;
/// The numbers' columns: loss and the five round-trip figures.
const NUMBER_WIDTH: f32 = 50.0;
const COLUMNS: [(&str, bool); 10] = [
    ("Hop", true),
    ("Host", false),
    ("Network", false),
    ("Loss", true),
    ("Last", true),
    ("Avg", true),
    ("Best", true),
    ("Worst", true),
    ("Jitter", true),
    ("Recent", false),
];

/// The traceroute shown in the route panel.
pub struct RouteView {
    pub host: HostId,
    pub trace: Trace,
}

/// A change asked for in the route panel, applied once the frame is drawn.
#[derive(Debug)]
pub enum RouteAction {
    Close,
    Restart,
    /// Watch a hop's address in a lane of its own.
    Watch(String),
    Copy(String),
    /// Give a network (by AS number) a name of the user's choosing.
    RenameNetwork(u32),
}

#[derive(Default)]
pub struct RouteOutput {
    pub actions: Vec<RouteAction>,
    /// How far the user dragged the panel's top edge up this frame; down is negative.
    pub resized_by: f32,
    /// The user double-clicked the edge, to fit the panel to the route again.
    pub fit: bool,
    /// How much taller the panel would have to be to show every row; negative
    /// when it has room to spare.
    pub missing_height: f32,
}

/// A line of the table: one hop, or a run of hops that never answer.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Row {
    Hop(usize),
    /// First and last hop of the run, as indexes.
    Silent(usize, usize),
}

pub fn show(
    ui: &mut Ui,
    view: &RouteView,
    host: &Host,
    networks: &Networks,
    clock: &Clock,
    p: &Palette,
) -> RouteOutput {
    let route = view.trace.read();
    let hops = route.shown_hops();
    let rows = rows(hops);
    let forwarded = forwarded_loss(hops, route.reached_at.is_some());
    let note = footnote(&route, hops, &forwarded);
    let mut out = RouteOutput::default();

    let handle = resize_handle(ui, p);
    out.resized_by = -handle.drag_delta().y;
    out.fit = handle.double_clicked();

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
            header(ui, &route, host, networks, clock, p, &mut out.actions);
            ui.add_space(4.0);
            // The footnote goes in first, along the bottom, so the table gets exactly
            // the space left and scrolls inside it.
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                if let Some(note) = &note {
                    ui.label(RichText::new(note).size(12.0).color(p.text_muted));
                    ui.add_space(4.0);
                }
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    out.missing_height = table(ui, &route, networks, &rows, &forwarded, p, &mut out.actions);
                });
            });
        });
    out
}

/// The gap above the panel: drag it to resize, double-click it to fit the route.
fn resize_handle(ui: &mut Ui, p: &Palette) -> Response {
    let height = GAP - ui.spacing().item_spacing.y;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click_and_drag());
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
        let color = if response.dragged() { p.accent } else { p.border_strong };
        let grip = Rect::from_center_size(rect.center(), vec2(40.0, 4.0));
        ui.painter().rect_filled(grip, CornerRadius::same(2), color);
    }
    response
        .on_hover_text("Drag to resize. Double-click to fit the route.")
        .accessible_name("Route panel size")
}

fn header(
    ui: &mut Ui,
    route: &Route,
    host: &Host,
    networks: &Networks,
    clock: &Clock,
    p: &Palette,
    actions: &mut Vec<RouteAction>,
) {
    egui::Sides::new().height(30.0).shrink_left().truncate().show(
        ui,
        |ui| {
            ui.label(
                RichText::new("Route to")
                    .font(theme::font(15.0))
                    .color(p.text_secondary),
            );
            ui.label(
                RichText::new(host.name())
                    .font(theme::font_semibold(15.0))
                    .color(p.text),
            );
            let mut summary = progress(route);
            let addr = route.target.to_string();
            if addr != host.name() {
                summary = format!("{addr}  ·  {summary}");
            }
            let path = network_path(route.shown_hops().iter().filter_map(Hop::addr), networks);
            if !path.is_empty() {
                let names: Vec<&str> = path.iter().map(|(_, name)| name.as_str()).collect();
                summary = format!("{summary}  ·  {}", names.join(" → "));
            }
            let label = ui.add(Label::new(RichText::new(summary).color(p.text_muted)).truncate());
            if !path.is_empty() {
                label.on_hover_text(
                    "The networks along the way, in order. The first is where your traffic \
                     leaves for the internet: your ISP, or whichever of your lines it took.",
                );
            }
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
                .on_hover_text("Stop tracing and close")
                .accessible_name("Close route")
                .clicked()
            {
                actions.push(RouteAction::Close);
            }
            if ui
                .button("Restart")
                .on_hover_text("Forget the numbers so far and start over")
                .clicked()
            {
                actions.push(RouteAction::Restart);
            }
            if ui
                .button("Copy report")
                .on_hover_text("Copy this table as text, e.g. for a ticket")
                .clicked()
            {
                actions.push(RouteAction::Copy(report(route, host, networks, clock)));
            }
        },
    );
}

/// The networks along a route's addresses in order, each once per stretch: AS
/// number and name.
fn network_path(addrs: impl Iterator<Item = IpAddr>, networks: &Networks) -> Vec<(u32, String)> {
    let mut path: Vec<(u32, String)> = Vec::new();
    for addr in addrs {
        let Some(Owner::Network(asn)) = networks.owner(addr) else {
            continue;
        };
        let name = networks.name(asn);
        if path.last().is_none_or(|(_, last)| *last != name) {
            path.push((asn, name));
        }
    }
    path
}

/// "reached in 11 hops", "tracing…" and the like.
fn progress(route: &Route) -> String {
    // The target answering clears what routers said before it, so a router saying
    // it can't reach the target is the newer news, as in the footnote.
    if let Some(hop) = route.unreachable_at() {
        format!("unreachable at hop {hop}")
    } else if let Some(hops) = route.reached_at {
        format!("reached in {hops} hop{}", if hops == 1 { "" } else { "s" })
    } else if route.searching() {
        "tracing…".to_string()
    } else {
        "no answer from the host".to_string()
    }
}

/// Draws the table and returns how much taller it would have to be to show every row.
fn table(
    ui: &mut Ui,
    route: &Route,
    networks: &Networks,
    rows: &[Row],
    forwarded: &[Option<f32>],
    p: &Palette,
    actions: &mut Vec<RouteAction>,
) -> f32 {
    let hops = route.shown_hops();
    // One scale for every hop, so delay visibly grows along the path.
    let scale_ms = series::latency_top(hops.iter().filter_map(|h| h.worst_ms).fold(0.0, f32::max)) as f32;
    let right = Layout::right_to_left(Align::Center);
    let body = TableBuilder::new(ui)
        .id_salt("route")
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::exact(40.0))
        .column(Column::remainder().at_least(130.0).clip(true))
        .column(Column::exact(NETWORK_WIDTH).clip(true))
        .columns(Column::exact(NUMBER_WIDTH), 6)
        .column(Column::exact(RECENT_RESULTS as f32 * 2.5))
        // Fill the panel, so the space to spare shows in the measurement.
        .auto_shrink([false, false])
        .min_scrolled_height(0.0)
        .header(HEADER_HEIGHT, |mut header| {
            for (title, numeric) in COLUMNS {
                header.col(|ui| {
                    let text = RichText::new(title).size(11.5).color(p.text_muted);
                    if numeric {
                        ui.with_layout(right, |ui| ui.label(text));
                    } else {
                        ui.label(text);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(ROW_HEIGHT, rows.len(), |mut row| match rows[row.index()] {
                Row::Hop(i) => hop_row(&mut row, route, networks, i, forwarded[i], scale_ms, p, actions),
                Row::Silent(first, last) => silent_row(&mut row, first, last, p),
            });
        });
    body.content_size.y - body.inner_rect.height()
}

#[allow(clippy::too_many_arguments)]
fn hop_row(
    row: &mut TableRow<'_, '_>,
    route: &Route,
    networks: &Networks,
    index: usize,
    real_loss: Option<f32>,
    scale_ms: f32,
    p: &Palette,
    actions: &mut Vec<RouteAction>,
) {
    let hop = &route.shown_hops()[index];
    let right = Layout::right_to_left(Align::Center);
    row.col(|ui| {
        ui.with_layout(right, |ui| {
            ui.label(RichText::new((index + 1).to_string()).color(p.text_muted))
        });
    });
    row.col(|ui| host_cell(ui, route, hop, p, actions));
    row.col(|ui| network_cell(ui, networks, hop, p, actions));
    row.col(|ui| {
        ui.with_layout(right, |ui| loss_cell(ui, hop, real_loss, p));
    });
    for value in [hop.last_ms, hop.avg_ms(), hop.best_ms, hop.worst_ms, hop.jitter_ms()] {
        row.col(|ui| {
            ui.with_layout(right, |ui| match value {
                Some(ms) => ui.label(RichText::new(format::ms(ms)).color(p.text)),
                None => dash(ui, p),
            });
        });
    }
    row.col(|ui| recent_strip(ui, hop, real_loss, scale_ms, p));
}

/// Several hops in a row that never answer, which big networks often have.
fn silent_row(row: &mut TableRow<'_, '_>, first: usize, last: usize, p: &Palette) {
    let right = Layout::right_to_left(Align::Center);
    let count = last - first + 1;
    row.col(|ui| {
        ui.with_layout(right, |ui| {
            ui.label(RichText::new(format!("{}–{}", first + 1, last + 1)).color(p.text_muted))
        });
    });
    row.col(|ui| {
        ui.label(RichText::new(format!("No reply  ·  {count} hops")).color(p.text_muted))
            .on_hover_text(format!(
                "None of these {count} routers answer traceroute probes. That's common \
                 inside big networks, and harmless when the hops after them answer."
            ));
    });
    row.col(|_| {});
    for _ in 0..6 {
        row.col(|ui| {
            ui.with_layout(right, |ui| dash(ui, p));
        });
    }
    row.col(|_| {});
}

fn dash(ui: &mut Ui, p: &Palette) -> Response {
    ui.label(RichText::new("—").color(p.text_muted))
}

fn host_cell(ui: &mut Ui, route: &Route, hop: &Hop, p: &Palette, actions: &mut Vec<RouteAction>) {
    let Some(addr) = hop.addr() else {
        let text = if hop.sent == 0 { "Waiting…" } else { "No reply" };
        ui.label(RichText::new(text).color(p.text_muted)).on_hover_text(
            "This router doesn't answer traceroute probes. That's common, and harmless \
             when the hops after it answer.",
        );
        return;
    };
    let ip = addr.to_string();
    let name = route.name(addr);
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut job = LayoutJob::default();
    let mut part = |text: &str, color| job.append(text, 0.0, TextFormat::simple(font.clone(), color));
    if name == ip {
        part(&ip, p.text);
    } else {
        part(&name, p.text);
        part(&format!("  {ip}"), p.text_muted);
    }
    if hop.addrs.len() > 1 {
        part(&format!("  +{}", hop.addrs.len() - 1), p.accent);
    }
    if hop.unreachable {
        part("  unreachable", p.critical);
    }

    let response = ui.add(Label::new(job).truncate().sense(Sense::click()));
    let response = response.on_hover_ui(|ui| {
        for answer in &hop.addrs {
            let who = match route.name(answer.addr) {
                name if name == answer.addr.to_string() => name,
                name => format!("{name}  {}", answer.addr),
            };
            let replies = answer.replies;
            ui.label(format!(
                "{who}  ({replies} {})",
                if replies == 1 { "reply" } else { "replies" }
            ));
        }
        if hop.addrs.len() > 1 {
            ui.label(
                RichText::new("Several routers answer at this distance: the path is load-balanced.")
                    .color(p.text_muted),
            );
        }
        if hop.unreachable {
            ui.label(RichText::new("This router reports that the host can't be reached.").color(p.critical));
        }
        ui.label(RichText::new("Right-click to watch or copy this hop.").color(p.text_muted));
    });
    response.context_menu(|ui| {
        if ui
            .button(format!("Watch {ip}"))
            .on_hover_text("Ping this hop in a lane of its own")
            .clicked()
        {
            actions.push(RouteAction::Watch(ip.clone()));
            ui.close();
        }
        if ui.button(format!("Copy {ip}")).clicked() {
            actions.push(RouteAction::Copy(ip.clone()));
            ui.close();
        }
        if name != ip && ui.button(format!("Copy {name}")).clicked() {
            actions.push(RouteAction::Copy(name.clone()));
            ui.close();
        }
    });
}

/// The network a hop's router belongs to, e.g. the ISP.
fn network_cell(ui: &mut Ui, networks: &Networks, hop: &Hop, p: &Palette, actions: &mut Vec<RouteAction>) {
    let Some(addr) = hop.addr() else {
        return;
    };
    let (text, tip) = match networks.owner(addr) {
        Some(Owner::Network(asn)) => return network_label(ui, networks, asn, p, actions),
        Some(Owner::Private) => (
            "Private",
            "A private address: it only means something inside its own network, so no ISP owns it.",
        ),
        Some(Owner::Unannounced) => (
            "Not announced",
            "A public address that no network announces on the internet. Some providers \
             number their routers that way.",
        ),
        None if networks.owner_failed(addr) => (
            "?",
            "Couldn't find out who owns this address. FerrisPing tries again in a minute.",
        ),
        None => ("…", "Finding out who owns this address"),
    };
    ui.add(Label::new(RichText::new(text).color(p.text_muted)).truncate())
        .on_hover_text(tip);
}

fn network_label(ui: &mut Ui, networks: &Networks, asn: u32, p: &Palette, actions: &mut Vec<RouteAction>) {
    let name = networks.name(asn);
    let response = ui.add(
        Label::new(RichText::new(&name).color(p.text_secondary))
            .truncate()
            .sense(Sense::click()),
    );
    let response = response.on_hover_ui(|ui| {
        ui.label(RichText::new(&name).font(theme::font_semibold(14.0)).color(p.text));
        ui.label(match networks.info(asn) {
            Some(info) => format!("AS{asn}  ·  {}", info.registered),
            None => format!("AS{asn}"),
        });
        ui.label(RichText::new("Right-click to rename this network or copy its number.").color(p.text_muted));
    });
    response.context_menu(|ui| {
        if ui
            .button(format!("Rename {name}…"))
            .on_hover_text("Call this network something else everywhere in FerrisPing")
            .clicked()
        {
            actions.push(RouteAction::RenameNetwork(asn));
            ui.close();
        }
        if ui.button(format!("Copy AS{asn}")).clicked() {
            actions.push(RouteAction::Copy(format!("AS{asn}")));
            ui.close();
        }
    });
}

fn loss_cell(ui: &mut Ui, hop: &Hop, real: Option<f32>, p: &Palette) {
    // Waiting for the first answer, or a hop that never answers: a percentage means nothing.
    let Some(loss) = hop.loss_pct().filter(|_| hop.answered()) else {
        dash(ui, p);
        return;
    };
    let color = match real {
        Some(real) if real >= 5.0 => p.critical,
        Some(real) if real > 0.0 => p.serious,
        Some(_) if loss > 0.0 => p.text_muted,
        _ => p.text,
    };
    let mut tip = format!("{} of {} probes went unanswered.", hop.sent - hop.received, hop.sent);
    if let Some(real) = real
        && loss - real >= RATE_LIMIT_MARGIN
    {
        let _ = write!(
            tip,
            " The hops after it lose {}, so the rest is this router limiting its \
             replies to traceroute, not traffic being dropped.",
            format::percent(real)
        );
    }
    ui.label(RichText::new(format::percent(loss)).color(color))
        .on_hover_text(tip);
}

/// The last probes to a hop, newest on the right: taller bars took longer, and a
/// full-height mark is a probe that got no answer.
fn recent_strip(ui: &mut Ui, hop: &Hop, real_loss: Option<f32>, scale_ms: f32, p: &Palette) {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT - 10.0), Sense::hover());
    let painter = ui.painter();
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.grid));
    let slot = rect.width() / RECENT_RESULTS as f32;
    let width = (slot - 1.0).max(1.0);
    // Misses only look alarming where the loss carries on to the host.
    let miss = if real_loss.is_some_and(|loss| loss > 0.0) {
        p.critical
    } else {
        p.border_strong
    };
    let first = RECENT_RESULTS - hop.recent.len();
    for (k, result) in hop.recent.iter().enumerate() {
        let left = rect.left() + (first + k) as f32 * slot;
        let (top, color) = match result {
            Some(ms) => (
                rect.bottom() - (ms / scale_ms).clamp(0.08, 1.0) * rect.height(),
                p.accent,
            ),
            None if hop.answered() => (rect.top(), miss),
            None => continue,
        };
        painter.rect_filled(
            Rect::from_min_max(pos2(left, top), pos2(left + width, rect.bottom())),
            0.0,
            color,
        );
    }
    response.on_hover_text(format!(
        "The last {RECENT_RESULTS} probes, newest on the right. Taller bars took longer \
         (full height is {}); a full-height grey or red bar got no answer.",
        format::ms_unit(scale_ms)
    ));
}

/// Runs of two or more hops that never answer share a row.
fn rows(hops: &[Hop]) -> Vec<Row> {
    let silent = |hop: &&Hop| hop.sent > 0 && !hop.answered();
    let mut rows = Vec::new();
    let mut i = 0;
    while i < hops.len() {
        let run = hops[i..].iter().take_while(silent).count();
        if run >= 2 {
            rows.push(Row::Silent(i, i + run - 1));
            i += run;
        } else {
            rows.push(Row::Hop(i));
            i += 1;
        }
    }
    rows
}

/// For each hop, the part of its loss that the hops after it share: traffic
/// really lost on the way. `None` where that can't be told: at hops that never
/// answer, and everywhere when the host itself doesn't answer.
///
/// Routers answer traceroute at low priority, so a hop can drop probes while
/// forwarding everything; only loss that carries on to the host is real.
fn forwarded_loss(hops: &[Hop], reached: bool) -> Vec<Option<f32>> {
    let mut forwarded = vec![None; hops.len()];
    if !reached {
        return forwarded;
    }
    let mut least_after: Option<f32> = None;
    for (i, hop) in hops.iter().enumerate().rev() {
        let Some(loss) = hop.loss_pct().filter(|_| hop.answered()) else {
            continue;
        };
        let least = least_after.map_or(loss, |l| l.min(loss));
        least_after = Some(least);
        forwarded[i] = Some(least);
    }
    forwarded
}

fn footnote(route: &Route, hops: &[Hop], forwarded: &[Option<f32>]) -> Option<String> {
    const PRIVATE: &str = "That's a private address, and the probes left your network looking for \
                           it: it's only reachable from inside its own network, or over a VPN.";
    let target = route.target;
    let router = |hop: u8| {
        hop.checked_sub(1)
            .and_then(|i| hops.get(i as usize))
            .and_then(Hop::addr)
    };
    // The last router heard from is out on the internet, so the probes for a
    // private address went the wrong way.
    let left_home = |hop: u8| is_private(target) && router(hop).is_some_and(|addr| !is_private(addr));
    if let Some(at) = route.unreachable_at() {
        let who = router(at).map_or_else(
            || format!("hop {at}"),
            |addr| format!("hop {at} ({})", route.name(addr)),
        );
        let why = if left_home(at) {
            PRIVATE
        } else {
            "When that's the last router before the host, the host is usually down or disconnected."
        };
        return Some(format!("The router at {who} says it can't reach {target}. {why}"));
    }
    if route.reached_at.is_none() && !route.searching() {
        let last = route.last_answered();
        return Some(if last == 0 {
            "Nothing on the way answers. A firewall may be dropping the probes.".to_string()
        } else if left_home(last) {
            format!("Nothing answers past hop {last}. {PRIVATE}")
        } else {
            format!("Nothing answers past hop {last}. A firewall there may drop the probes, or the host is down.")
        });
    }
    let rate_limited = hops
        .iter()
        .zip(forwarded)
        .any(|(hop, real)| match (hop.loss_pct(), real) {
            (Some(loss), Some(real)) => loss - real >= RATE_LIMIT_MARGIN,
            _ => false,
        });
    rate_limited.then(|| {
        "Grey loss isn't shared by the hops after it, so traffic gets through: those routers just skip \
         some traceroute replies."
            .to_string()
    })
}

/// Addresses that only mean something inside one network.
fn is_private(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_unique_local() || v6.is_unicast_link_local() || v6.is_loopback(),
    }
}

/// The table as plain text, for pasting into a ticket or a chat.
fn report(route: &Route, host: &Host, networks: &Networks, clock: &Clock) -> String {
    let hops = route.shown_hops();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "FerrisPing route to {} ({}) since {}: {}",
        host.name(),
        route.target,
        clock.time(route.started),
        progress(route)
    );
    let path = network_path(hops.iter().filter_map(Hop::addr), networks);
    if !path.is_empty() {
        let names: Vec<String> = path.iter().map(|(asn, name)| format!("{name} (AS{asn})")).collect();
        let _ = writeln!(out, "Networks: {}", names.join(" -> "));
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{:>5}  {:<56} {:<24} {:>5} {:>5} {:>6} {:>6} {:>6} {:>6} {:>6}",
        "Hop", "Host", "Network", "Loss", "Sent", "Last", "Avg", "Best", "Worst", "Jitter"
    );
    let ms = |value: Option<f32>| value.map_or_else(|| "-".to_string(), format::ms);
    for row in rows(hops) {
        let i = match row {
            Row::Hop(i) => i,
            Row::Silent(first, last) => {
                let _ = writeln!(out, "{:>5}  * (no reply)", format!("{}-{}", first + 1, last + 1));
                continue;
            }
        };
        let hop = &hops[i];
        let mut who = match hop.addr() {
            Some(addr) => match route.name(addr) {
                name if name == addr.to_string() => name,
                name => format!("{name} [{addr}]"),
            },
            None => "*".to_string(),
        };
        if hop.addrs.len() > 1 {
            let _ = write!(who, " +{}", hop.addrs.len() - 1);
        }
        if hop.unreachable {
            who.push_str(" unreachable");
        }
        let loss = hop
            .loss_pct()
            .filter(|_| hop.answered())
            .map_or_else(|| "-".to_string(), format::percent);
        let network = match hop.addr().and_then(|addr| networks.owner(addr)) {
            Some(Owner::Network(asn)) => format!("{} AS{asn}", networks.name(asn)),
            Some(Owner::Private) => "private".to_string(),
            Some(Owner::Unannounced) => "not announced".to_string(),
            None => String::new(),
        };
        let _ = writeln!(
            out,
            "{:>5}  {:<56} {:<24} {:>5} {:>5} {:>6} {:>6} {:>6} {:>6} {:>6}",
            i + 1,
            who,
            network,
            loss,
            hop.sent,
            ms(hop.last_ms),
            ms(hop.avg_ms()),
            ms(hop.best_ms),
            ms(hop.worst_ms),
            ms(hop.jitter_ms())
        );
    }
    if let Some(note) = footnote(route, hops, &forwarded_loss(hops, route.reached_at.is_some())) {
        let _ = writeln!(out);
        let _ = writeln!(out, "{note}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(sent: u32, received: u32) -> Hop {
        let mut hop = Hop::default();
        hop.sent = sent;
        hop.received = received;
        hop
    }

    #[test]
    fn only_loss_the_later_hops_share_is_real() {
        // A silent router, one that limits its replies, and the host losing a little.
        let hops = [hop(10, 0), hop(10, 5), hop(10, 9)];
        assert_eq!(forwarded_loss(&hops, true), [None, Some(10.0), Some(10.0)]);
    }

    #[test]
    fn loss_that_starts_further_on_isnt_blamed_on_earlier_hops() {
        let hops = [hop(10, 10), hop(10, 8)];
        assert_eq!(forwarded_loss(&hops, true), [Some(0.0), Some(20.0)]);
    }

    #[test]
    fn without_the_hosts_answers_no_loss_is_judged() {
        let hops = [hop(10, 10), hop(10, 2)];
        assert_eq!(forwarded_loss(&hops, false), [None, None]);
    }

    #[test]
    fn runs_of_silent_hops_share_a_row() {
        let hops = [
            hop(5, 0),
            hop(5, 5),
            hop(5, 0),
            hop(5, 0),
            hop(5, 0),
            hop(5, 5),
            hop(0, 0),
        ];
        assert_eq!(
            rows(&hops),
            [Row::Hop(0), Row::Hop(1), Row::Silent(2, 4), Row::Hop(5), Row::Hop(6)]
        );
    }

    #[test]
    fn network_path_names_each_stretch_once() {
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        let mut networks = Networks::default();
        let comcast = "COMCAST-7922 - Comcast Cable Communications, LLC, US";
        let arelion = "TWELVE99 Arelion, fka Telia Carrier, SE";
        networks.insert_for_test(ip("68.86.0.1"), 7922, comcast);
        networks.insert_for_test(ip("62.0.0.1"), 1299, arelion);
        networks.insert_for_test(ip("62.0.0.2"), 1299, arelion);
        networks.insert_for_test(ip("1.1.1.1"), 13335, "CLOUDFLARENET - Cloudflare, Inc., US");
        // A private hop, and one whose owner isn't known yet, don't break a stretch.
        let addrs = ["10.0.0.1", "68.86.0.1", "62.0.0.1", "9.9.9.9", "62.0.0.2", "1.1.1.1"].map(ip);
        let path = network_path(addrs.into_iter(), &networks);
        assert_eq!(
            path,
            [
                (7922, "Comcast".to_string()),
                (1299, "Arelion".to_string()),
                (13335, "Cloudflare".to_string())
            ]
        );
    }

    #[test]
    fn private_addresses() {
        let private = |text: &str| is_private(text.parse().unwrap());
        assert!(private("10.0.0.1"));
        assert!(private("192.168.1.1"));
        assert!(private("fd00::1"));
        assert!(!private("4.2.2.2"));
        assert!(!private("2606:4700::1111"));
    }
}
