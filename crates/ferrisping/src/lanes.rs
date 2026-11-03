//! The main view: one lane per host, its numbers on the left and a live latency
//! graph on the right. All graphs share one time axis, pinned above the lanes.
//! With many hosts the lanes get shorter, then compact: the numbers move to a
//! tooltip, so a dozen hosts fit on a laptop screen.

use eframe::egui::{
    self, Align, Align2, Button, CornerRadius, Frame, Id, Label, Layout, Rect, Response, RichText, Sense, Stroke, Ui,
    UiBuilder, pos2, vec2,
};
use egui_plot::{
    AxisHints, GridInput, HPlacement, HoverPosition, Line, LineStyle, Plot, PlotPoint, PlotPoints, PlotTransform,
    Points, Span, Text, VLine,
};
use ferrisping_core::{Exit, History, Host, HostId, HostStatus, Networks, Outcome, State, WindowStats};

use crate::format::{self, Clock};
use crate::series;
use crate::theme::{self, Palette};

const INFO_WIDTH: f32 = 300.0;
/// How long a host's way out stays highlighted after it switched.
const RECENT_SWITCH_SECS: f64 = 15.0 * 60.0;
const HEADER_HEIGHT: f32 = 30.0;
const MAX_LANE_HEIGHT: f32 = 170.0;
/// The shortest lane with room for the numbers.
const MIN_LANE_HEIGHT: f32 = 90.0;
/// The shortest compact lane. A dozen fit on a laptop screen: 1280 × 800 points,
/// less the taskbar and the title bar.
const MIN_COMPACT_HEIGHT: f32 = 46.0;
/// The height of a full lane's text, and of a compact lane's.
const FULL_TEXT_HEIGHT: f32 = 86.0;
const COMPACT_TEXT_HEIGHT: f32 = 39.0;
const Y_AXIS_WIDTH: f32 = 30.0;
/// The plot's minimum grid spacing in points. egui_plot passes the grid spacer
/// `base_step_size` = plot units per point × this, so the spacer divides it back out.
const GRID_MIN_SPACING: f32 = 8.0;

/// The time range to draw this frame.
pub struct View {
    pub now: f64,
    /// Visible range. In live mode the graphs are set to it; otherwise the graphs
    /// keep the range the user dragged to, and this is last frame's copy of it.
    pub t0: f64,
    pub t1: f64,
    pub live: bool,
    /// Current probe interval in seconds.
    pub interval: f64,
}

/// A change the user asked for, applied after the state lock is released.
#[derive(Debug)]
pub enum LaneAction {
    Remove(HostId),
    Move(HostId, usize),
    Copy(String),
    TraceRoute(HostId),
    /// Give a network (by AS number) a name of the user's choosing.
    RenameNetwork(u32),
    /// Open the host's Alerts dialog.
    Alerts(HostId),
}

/// A switch to another way out, ready to draw on a graph.
struct Switch {
    at: f64,
    to: String,
    from: String,
}

#[derive(Default)]
pub struct LanesOutput {
    pub actions: Vec<LaneAction>,
    /// The user dragged or zoomed a graph, so stop following the live edge.
    pub left_live: bool,
    /// The user double-clicked a graph to jump back to live.
    pub go_live: bool,
    /// The visible time range after this frame's interaction.
    pub bounds: Option<(f64, f64)>,
}

/// The lanes' height with every lane at full height, which is as tall as they get.
pub fn natural_height(count: usize) -> f32 {
    HEADER_HEIGHT + count as f32 * MAX_LANE_HEIGHT + 2.0
}

/// What a lane has room to show.
#[derive(Clone, Copy, Debug, PartialEq)]
enum LaneStyle {
    /// Name, status, and the numbers for the visible time range.
    Full,
    /// Name and status, with the numbers in the name's tooltip.
    Compact,
}

/// Each lane's height and style when `count` lanes share `room`, the height of
/// their card. Lanes shrink to fit, then go compact, and past that the list scrolls.
fn lane_size(room: f32, count: usize) -> (f32, LaneStyle) {
    let share = ((room - HEADER_HEIGHT - 2.0) / count.max(1) as f32).floor();
    if share >= MIN_LANE_HEIGHT {
        (share.min(MAX_LANE_HEIGHT), LaneStyle::Full)
    } else {
        (share.max(MIN_COMPACT_HEIGHT), LaneStyle::Compact)
    }
}

pub fn show(ui: &mut Ui, state: &State, view: &View, clock: &Clock, p: &'static Palette) -> LanesOutput {
    let mut out = LanesOutput::default();
    let count = state.hosts.len();
    let (lane_height, style) = lane_size(ui.available_height(), count);

    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(10))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            let (header, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_HEIGHT), Sense::hover());
            let mut axis: Option<PlotTransform> = None;
            egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                for (index, host) in state.hosts.iter().enumerate() {
                    let transform = lane(
                        ui,
                        host,
                        &state.networks,
                        index,
                        count,
                        lane_height,
                        style,
                        view,
                        clock,
                        p,
                        &mut out,
                    );
                    axis = axis.or(transform);
                }
            });
            if let Some(transform) = &axis {
                let bounds = transform.bounds();
                out.bounds = Some((bounds.min()[0], bounds.max()[0]));
            }
            paint_header(ui, header, axis.as_ref(), clock, p);
        });
    out
}

#[allow(clippy::too_many_arguments)]
fn lane(
    ui: &mut Ui,
    host: &Host,
    networks: &Networks,
    index: usize,
    count: usize,
    height: f32,
    style: LaneStyle,
    view: &View,
    clock: &Clock,
    p: &'static Palette,
    out: &mut LanesOutput,
) -> Option<PlotTransform> {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let painter = ui.painter().clone();
    if index > 0 {
        painter.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, p.border));
    }
    let inset = (height * 0.15).clamp(6.0, 14.0);
    let stripe = Rect::from_min_max(
        pos2(rect.left() + 10.0, rect.top() + inset),
        pos2(rect.left() + 13.0, rect.bottom() - inset),
    );
    painter.rect_filled(stripe, CornerRadius::same(2), theme::status_color(p, host.status));

    // Short lanes bring their text and graph closer to the edges. The graph keeps
    // room for its scale's labels, which stand half out of it.
    let (text_top, graph_inset) = match style {
        LaneStyle::Full => ((height - FULL_TEXT_HEIGHT).clamp(4.0, 10.0), 9.0),
        LaneStyle::Compact => (((height - COMPACT_TEXT_HEIGHT) / 2.0).clamp(3.0, 10.0), 8.0),
    };
    let info_rect = Rect::from_min_max(
        pos2(rect.left() + 26.0, rect.top() + text_top),
        pos2(rect.left() + INFO_WIDTH, rect.bottom() - 8.0),
    );
    let plot_rect = Rect::from_min_max(
        pos2(rect.left() + INFO_WIDTH + 16.0, rect.top() + graph_inset),
        pos2(rect.right() - 10.0, rect.bottom() - graph_inset),
    );

    let hovered = ui.rect_contains_pointer(rect);
    let stats = host.history.stats(view.t0, view.t1.min(view.now));
    info(
        ui, info_rect, host, networks, &stats, style, view.now, hovered, clock, p, out,
    );
    let plot = graph(ui, plot_rect, host, networks, style, view, clock, p, out);
    lane_menu(&response, host, networks, index, count, out);
    if let Some(plot) = &plot {
        lane_menu(&plot.0, host, networks, index, count, out);
    }
    plot.map(|(_, transform)| transform)
}

#[allow(clippy::too_many_arguments)]
fn info(
    ui: &mut Ui,
    rect: Rect,
    host: &Host,
    networks: &Networks,
    stats: &WindowStats,
    style: LaneStyle,
    now: f64,
    hovered: bool,
    clock: &Clock,
    p: &Palette,
    out: &mut LanesOutput,
) {
    let compact = style == LaneStyle::Compact;
    let mut ui = ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)));
    // Tight rows: the lane is short and the graph next to it carries the detail.
    ui.spacing_mut().item_spacing = vec2(6.0, if compact { 1.0 } else { 2.0 });
    ui.spacing_mut().interact_size.y = 16.0;
    // Plain text, so a right-click anywhere on the lane opens its menu instead of
    // being taken by a text selection.
    ui.style_mut().interaction.selectable_labels = false;

    // Name, with the latest round trip on the right. A compact lane has no room
    // for the numbers, so the name's tooltip has them.
    let (name_size, ms_size) = if compact { (14.0, 15.0) } else { (15.0, 19.0) };
    egui::Sides::new()
        .height(if compact { 20.0 } else { 24.0 })
        .shrink_left()
        .truncate()
        .show(
            &mut ui,
            |ui| {
                let (icon, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                theme::paint_status_icon(ui.painter(), icon.center(), 11.0, host.status, p);
                let name = RichText::new(host.name())
                    .font(theme::font_semibold(name_size))
                    .color(p.text);
                let name = ui.add(Label::new(name).truncate());
                if compact {
                    name.on_hover_ui(|ui| {
                        ui.label(full_name(host));
                        ui.add_space(4.0);
                        numbers(ui, ("numbers-tooltip", host.id.0), stats, p);
                    });
                } else {
                    name.on_hover_text(full_name(host));
                }
            },
            |ui| {
                let latest = host.history.last_finished().and_then(|s| s.rtt_ms());
                match latest {
                    Some(ms) => {
                        ui.label(RichText::new("ms").size(12.0).color(p.text_muted));
                        ui.label(
                            RichText::new(format::ms(ms))
                                .font(theme::font_semibold(ms_size))
                                .color(p.text),
                        );
                    }
                    None => {
                        ui.label(
                            RichText::new("—")
                                .font(theme::font_semibold(ms_size))
                                .color(p.text_muted),
                        );
                    }
                }
            },
        );

    // Status, the network the traffic leaves through and the address, with buttons
    // while the pointer is over the lane. The address goes last, and only whole: it's
    // in the name's tooltip too, and a cut-off address says nothing.
    egui::Sides::new().height(18.0).shrink_left().truncate().show(
        &mut ui,
        |ui| {
            ui.add_space(18.0);
            ui.spacing_mut().item_spacing.x = 0.0;
            let line = |text: String| RichText::new(text).size(13.0).color(p.text_secondary);
            ui.add(Label::new(line(status_text(host, now))).truncate());
            if let Some(exit) = &host.exit {
                ui.add(Label::new(line("  ·  ".into())));
                exit_label(ui, host, exit, networks, now, clock, p);
            }
            if let Some(addr) = host.addr.map(|a| a.to_string()).filter(|a| *a != host.name()) {
                let text = egui::WidgetText::from(line(format!("  ·  {addr}")));
                let galley = text.into_galley(
                    ui,
                    Some(egui::TextWrapMode::Truncate),
                    ui.available_width(),
                    egui::FontSelection::Default,
                );
                if !galley.elided {
                    ui.add(Label::new(galley));
                }
            }
        },
        |ui| {
            if hovered {
                // No taller than the row, or showing them would push the numbers
                // below down, over the lane's edge when lanes are short.
                ui.spacing_mut().button_padding.y = 0.0;
                let remove = Button::new(
                    RichText::new(theme::icon(theme::ICON_CLOSE, "×"))
                        .size(11.0)
                        .color(p.text_muted),
                )
                .frame_when_inactive(false)
                .min_size(vec2(22.0, 18.0));
                if ui
                    .add(remove)
                    .on_hover_text("Stop watching this host")
                    .accessible_name(format!("Remove {}", host.name()))
                    .clicked()
                {
                    out.actions.push(LaneAction::Remove(host.id));
                }
                if host.addr.is_some() {
                    let route = Button::new(RichText::new("Route").size(12.5).color(p.text_secondary))
                        .frame_when_inactive(false)
                        .min_size(vec2(0.0, 18.0));
                    if ui
                        .add(route)
                        .on_hover_text("Trace the route to this host, hop by hop")
                        .accessible_name(format!("Trace route to {}", host.name()))
                        .clicked()
                    {
                        out.actions.push(LaneAction::TraceRoute(host.id));
                    }
                }
            }
        },
    );

    if !compact {
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            ui.add_space(18.0);
            numbers(ui, ("stats", host.id.0), stats, p);
        });
    }
}

/// Average, best, worst, jitter and loss over the visible time range.
fn numbers(ui: &mut Ui, id: impl egui::AsIdSalt, stats: &WindowStats, p: &Palette) {
    let dash = || "—".to_string();
    let values = [
        stats.avg_ms.map(format::ms).unwrap_or_else(dash),
        stats.min_ms.map(format::ms).unwrap_or_else(dash),
        stats.max_ms.map(format::ms).unwrap_or_else(dash),
        stats.jitter_ms.map(format::ms).unwrap_or_else(dash),
        stats.loss_pct().map(format::percent).unwrap_or_else(dash),
    ];
    egui::Grid::new(id)
        .num_columns(5)
        .min_col_width(40.0)
        .min_row_height(15.0)
        .spacing(vec2(12.0, 0.0))
        .show(ui, |ui| {
            for label in ["Avg", "Min", "Max", "Jitter", "Loss"] {
                ui.label(RichText::new(label).size(11.5).color(p.text_muted));
            }
            ui.end_row();
            for value in values {
                ui.label(RichText::new(value).size(13.5).color(p.text_secondary));
            }
            ui.end_row();
        });
}

fn full_name(host: &Host) -> String {
    let mut text = host.spec.host.clone();
    if let Some(label) = &host.spec.label {
        text = format!("{label}\n{text}");
    }
    if let Some(addr) = host.addr.filter(|a| a.to_string() != host.spec.host) {
        text.push_str(&format!(" ({addr})"));
    }
    text
}

/// "via Lumen": the network the host's traffic leaves through, highlighted for a
/// while after a switch, with the details on hover.
fn exit_label(ui: &mut Ui, host: &Host, exit: &Exit, networks: &Networks, now: f64, clock: &Clock, p: &Palette) {
    let name = networks.name(exit.asn);
    // The switch that led here, if FerrisPing saw one.
    let switch = host.exit_changes.last().filter(|change| change.at == exit.since);
    let recent = switch.is_some() && now - exit.since < RECENT_SWITCH_SECS;
    let color = if recent { p.accent } else { p.text_secondary };
    let text = RichText::new(format!("via {name}")).size(13.0).color(color);
    ui.add(Label::new(text).truncate()).on_hover_ui(|ui| {
        ui.label(
            RichText::new(format!("Traffic leaves through {name}"))
                .font(theme::font_semibold(14.0))
                .color(p.text),
        );
        ui.label(match networks.info(exit.asn) {
            Some(info) => format!("AS{}  ·  {}", exit.asn, info.registered),
            None => format!("AS{}", exit.asn),
        });
        let hops = if exit.hop == 1 {
            "1 hop".to_string()
        } else {
            format!("{} hops", exit.hop)
        };
        ui.label(format!("Its first router on the way is {}, {hops} out.", exit.router));
        ui.label(match switch {
            Some(change) => format!(
                "Switched from {} at {}.",
                networks.name(change.from),
                clock.time(change.at)
            ),
            None => format!("Seen leaving this way since {}.", clock.time(exit.since)),
        });
        ui.label(
            RichText::new(format!(
                "It's the first public network on the route: usually, but not always, the ISP this \
                 traffic goes out through. Checked every {} unless the host is down. Right-click the lane \
                 to rename this network.",
                format::interval(ferrisping_core::EXIT_CHECK_EVERY)
            ))
            .color(p.text_muted),
        );
    });
}

fn status_text(host: &Host, now: f64) -> String {
    let since = format::duration(now - host.status_since);
    match host.status {
        HostStatus::Resolving => "Looking up address".into(),
        HostStatus::ResolveFailed => "Can't resolve".into(),
        HostStatus::Waiting => "Waiting for a reply".into(),
        HostStatus::Up => format!("Up {since}"),
        HostStatus::Down => format!("Down {since}"),
    }
}

/// The latency graph. Returns its response and its screen transform.
#[allow(clippy::too_many_arguments)]
fn graph(
    ui: &mut Ui,
    rect: Rect,
    host: &Host,
    networks: &Networks,
    style: LaneStyle,
    view: &View,
    clock: &Clock,
    p: &'static Palette,
    out: &mut LanesOutput,
) -> Option<(Response, PlotTransform)> {
    let mut plot_ui = ui.new_child(UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)));
    let history: &History = &host.history;
    let interval = view.interval;
    let grid_clock = clock.clone();
    let points_wide = (rect.width() - Y_AXIS_WIDTH).max(50.0);
    let secs_per_point = (view.t1 - view.t0) / points_wide as f64;
    let (live, t0, t1) = (view.live, view.t0, view.t1);
    let name = host.name().to_string();
    let link = Id::unique("ferrisping-lanes");
    let switches: Vec<Switch> = host
        .exit_changes
        .iter()
        .map(|change| Switch {
            at: change.at,
            to: networks.name(change.to),
            from: networks.name(change.from),
        })
        .collect();
    let switches = &switches;
    // A compact lane's graph is too short for a label half way up: egui_plot hides
    // labels that crowd together, all of them, and the graph would have no scale.
    let steps = match style {
        LaneStyle::Full => 2,
        LaneStyle::Compact => 1,
    };

    let plot = Plot::new(("lane", host.id.0))
        .width(rect.width())
        .height(rect.height())
        .show_background(false)
        .show_axes([false, true])
        .show_grid([true, true])
        .grid_color(p.grid)
        .grid_spacing(GRID_MIN_SPACING..=300.0)
        .x_grid_spacer(move |input: GridInput| {
            let secs_per_point = input.base_step_size / GRID_MIN_SPACING as f64;
            series::time_marks(input.bounds.0, input.bounds.1, secs_per_point, |t| {
                grid_clock.utc_offset_secs(t)
            })
        })
        .y_grid_spacer(move |input: GridInput| series::latency_marks(input.bounds.1, steps))
        // Latency scale on the right, next to the newest samples.
        .custom_y_axes(vec![
            AxisHints::new_y()
                .formatter(|mark, _| series::latency_tick(mark.value))
                .min_thickness(Y_AXIS_WIDTH)
                .placement(HPlacement::Right)
                .tick_label_color(p.text_muted)
                .tick_label_font(theme::font(11.5)),
        ])
        .link_axis(link, [true, false])
        .link_cursor(link, [true, false])
        .show_x(true)
        .show_y(false)
        .cursor_color(p.text_muted)
        .allow_scroll(false)
        .allow_zoom([true, false])
        .allow_drag([true, false])
        .allow_axis_zoom_drag(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .set_margin_fraction(vec2(0.0, 0.0))
        .label_formatter(move |pos| hover_text(pos, history, switches, clock, interval, secs_per_point));

    let line_color = p.accent;
    let loss_fill = p.critical.gamma_multiply(0.32);
    let marker = p.text_secondary;
    let response = plot.show(&mut plot_ui, move |pui| {
        if live {
            pui.set_plot_bounds_x(t0..=t1);
        }
        let (lo, hi) = if live {
            (t0, t1)
        } else {
            let bounds = pui.plot_bounds();
            (bounds.min()[0], bounds.max()[0])
        };
        let series = series::build(history, lo, hi, points_wide as usize, interval);
        let top = series::latency_top(series.max_ms);
        pui.set_plot_bounds_y(0.0..=top);
        // A switch to another network: a dashed line where it happened, named at the
        // top. Close to the right edge the name goes on the left of the line.
        for (i, switch) in switches.iter().enumerate().filter(|(_, s)| (lo..=hi).contains(&s.at)) {
            pui.vline(
                VLine::new("", switch.at)
                    .id(("switch", i))
                    .color(marker)
                    .width(1.0)
                    .style(LineStyle::dashed_dense())
                    .allow_hover(false),
            );
            let name = format!("to {}", switch.to);
            // Points right of the line, against a generous guess at the name's width.
            let room = (hi - switch.at) / (hi - lo).max(1e-9) * points_wide as f64;
            let (anchor, text) = if room > name.chars().count() as f64 * 6.5 + 12.0 {
                (Align2::LEFT_TOP, format!(" {name}"))
            } else {
                (Align2::RIGHT_TOP, format!("{name} "))
            };
            pui.text(
                Text::new("", PlotPoint::new(switch.at, top), RichText::new(text).size(11.5))
                    .id(("switch-name", i))
                    .anchor(anchor)
                    .color(marker)
                    .allow_hover(false),
            );
        }
        for (start, end) in series.losses {
            pui.span(Span::new("", start..=end).fill(loss_fill));
        }
        for (i, run) in series.runs.into_iter().enumerate() {
            if run.len() == 1 {
                pui.points(
                    Points::new(name.clone(), PlotPoints::from(run))
                        .id(("dot", i))
                        .radius(1.5)
                        .color(line_color),
                );
            } else {
                pui.line(
                    Line::new(name.clone(), PlotPoints::from(run))
                        .id(("run", i))
                        .color(line_color)
                        .width(1.75)
                        .fill(0.0)
                        .fill_alpha(0.12),
                );
            }
        }
    });

    if history.last_finished().is_none() {
        let message = match host.status {
            HostStatus::Resolving => "Looking up the address…".to_string(),
            HostStatus::ResolveFailed => host
                .resolve_error
                .clone()
                .unwrap_or_else(|| "Can't resolve this name".into()),
            _ => "Waiting for the first reply…".to_string(),
        };
        ui.painter().text(
            response.transform.frame().center(),
            Align2::CENTER_CENTER,
            message,
            theme::font(13.0),
            p.text_muted,
        );
    }

    let r = &response.response;
    if r.dragged() || (r.hovered() && ui.input(|i| i.zoom_delta() != 1.0)) {
        out.left_live = true;
    }
    if r.double_clicked() {
        out.go_live = true;
    }
    Some((response.response, response.transform))
}

fn hover_text(
    pos: &HoverPosition<'_>,
    history: &History,
    switches: &[Switch],
    clock: &Clock,
    interval: f64,
    secs_per_point: f64,
) -> Option<String> {
    let t = match *pos {
        HoverPosition::NearDataPoint { position, .. } | HoverPosition::Elsewhere { position } => position.x,
    };
    let max_dt = (interval * 0.75).max(secs_per_point * 4.0);
    let text = match series::nearest(history, t, max_dt) {
        Some(sample) => match sample.outcome {
            Outcome::Reply { rtt_ms } => Some(format!("{}\n{}", format::ms_unit(rtt_ms), clock.time(sample.t))),
            Outcome::Lost(status) => Some(format!("No reply: {}\n{}", status.label(), clock.time(sample.t))),
            Outcome::Pending => None,
        },
        None => Some(clock.time(t)),
    };
    match switches.iter().find(|s| (s.at - t).abs() <= max_dt) {
        Some(switch) => {
            let note = format!("Switched to {} from {}", switch.to, switch.from);
            Some(text.map_or_else(|| note.clone(), |text| format!("{note}\n{text}")))
        }
        None => text,
    }
}

fn lane_menu(response: &Response, host: &Host, networks: &Networks, index: usize, count: usize, out: &mut LanesOutput) {
    response.context_menu(|ui| {
        if ui
            .add_enabled(host.addr.is_some(), Button::new("Trace route"))
            .on_hover_text("Find every hop on the way to this host and keep measuring each one")
            .clicked()
        {
            out.actions.push(LaneAction::TraceRoute(host.id));
            ui.close();
        }
        if let Some(exit) = &host.exit
            && ui
                .button(format!("Rename {}…", networks.name(exit.asn)))
                .on_hover_text(
                    "Call the network this host's traffic leaves through something else, everywhere in FerrisPing",
                )
                .clicked()
        {
            out.actions.push(LaneAction::RenameNetwork(exit.asn));
            ui.close();
        }
        if ui
            .button("Alerts…")
            .on_hover_text("Which of this host's events show a notification, and limits of its own")
            .clicked()
        {
            out.actions.push(LaneAction::Alerts(host.id));
            ui.close();
        }
        ui.separator();
        let address = host
            .addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| host.spec.host.clone());
        if ui.button(format!("Copy {address}")).clicked() {
            out.actions.push(LaneAction::Copy(address));
            ui.close();
        }
        ui.separator();
        if ui.add_enabled(index > 0, Button::new("Move up")).clicked() {
            out.actions.push(LaneAction::Move(host.id, index - 1));
            ui.close();
        }
        if ui.add_enabled(index + 1 < count, Button::new("Move down")).clicked() {
            out.actions.push(LaneAction::Move(host.id, index + 1));
            ui.close();
        }
        ui.separator();
        if ui.button("Remove").clicked() {
            out.actions.push(LaneAction::Remove(host.id));
            ui.close();
        }
    });
}

/// Column title over the host info, and time labels over the graphs that line up
/// with the graphs' grid lines.
fn paint_header(ui: &Ui, rect: Rect, axis: Option<&PlotTransform>, clock: &Clock, p: &Palette) {
    let painter = ui.painter();
    painter.text(
        pos2(rect.left() + 26.0, rect.center().y + 1.0),
        Align2::LEFT_CENTER,
        "HOST",
        theme::font_semibold(11.0),
        p.text_muted,
    );
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.border));
    let Some(transform) = axis else {
        return;
    };
    let frame = *transform.frame();
    let bounds = transform.bounds();
    let (lo, hi) = (bounds.min()[0], bounds.max()[0]);
    let secs_per_point = (hi - lo) / frame.width().max(1.0) as f64;
    let step = series::label_step(secs_per_point);
    for (t, _) in series::aligned(lo, hi, step, |t| clock.utc_offset_secs(t)) {
        let x = transform.position_from_point_x(t);
        let text = if step >= 86_400.0 {
            clock.day(t)
        } else if step >= 60.0 {
            clock.time_short(t)
        } else {
            clock.time(t)
        };
        let galley = painter.layout_no_wrap(text, theme::font(12.0), p.text_muted);
        let half = galley.size().x / 2.0;
        // Only whole labels: one cut off at either edge reads as a different time.
        if x - half < frame.left() || x + half > frame.right() {
            continue;
        }
        let top = rect.center().y - galley.size().y / 2.0;
        painter.galley(pos2(x - half, top), galley, p.text_muted);
        painter.vline(
            x,
            (rect.bottom() - 5.0)..=(rect.bottom() - 1.0),
            Stroke::new(1.0, p.axis),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `count` lanes fit in `room` without scrolling.
    fn fit(room: f32, count: usize) -> bool {
        let (height, _) = lane_size(room, count);
        HEADER_HEIGHT + 2.0 + count as f32 * height <= room
    }

    #[test]
    fn lanes_shrink_then_go_compact_then_scroll() {
        // The usual window, 820 points tall, gives the lanes 697.
        assert_eq!(lane_size(697.0, 2), (170.0, LaneStyle::Full));
        assert_eq!(lane_size(697.0, 7), (95.0, LaneStyle::Full));
        assert_eq!(lane_size(697.0, 8), (83.0, LaneStyle::Compact));
        assert_eq!(lane_size(697.0, 20), (MIN_COMPACT_HEIGHT, LaneStyle::Compact));
        assert!(fit(697.0, 14));
        assert!(!fit(697.0, 20), "the shortest lanes scroll");
    }

    #[test]
    fn a_laptop_screen_fits_six_full_lanes_or_a_dozen_compact_ones() {
        // 1920 × 1200 at 150% is 1280 × 800 points. Less the taskbar, the title bar,
        // and FerrisPing's toolbar and status bar, that leaves the lanes 589.
        assert_eq!(lane_size(589.0, 6).1, LaneStyle::Full);
        assert!(fit(589.0, 6));
        assert!(fit(589.0, 12));
    }
}
