//! The event log: when each host stopped answering and when it came back, when its
//! traffic started leaving through another network (ISP), and when its loss or
//! round trips crossed the limits set for alerts.

use std::sync::Arc;

use eframe::egui::text::{LayoutJob, TextWrapping};
use eframe::egui::{self, Color32, FontId, Galley, Painter, RichText, Sense, Stroke, Ui, pos2, vec2};
use ferrisping_core::{Event, EventKind, HostStatus, Networks, State};

use crate::format::{self, Clock};
use crate::theme::{self, Palette};

const ROW_HEIGHT: f32 = 50.0;

pub fn show(ui: &mut Ui, state: &State, clock: &Clock, p: &Palette) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Events").font(theme::font_semibold(15.0)).color(p.text));
        if !state.events.is_empty() {
            ui.label(RichText::new(state.events.len().to_string()).color(p.text_muted));
        }
    });
    ui.add_space(6.0);
    if state.events.is_empty() {
        // Switches are only found while routes are checked.
        let text = if state.settings.network_lookups {
            "Nothing yet. A host that misses 3 replies in a row is logged here, \
             and again when it answers. So is traffic switching to another ISP."
        } else {
            "Nothing yet. A host that misses 3 replies in a row is logged here, \
             and again when it answers."
        };
        ui.label(RichText::new(text).color(p.text_muted));
        return;
    }

    let events = &state.events;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, events.len(), |ui, rows| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for i in rows {
                // Newest first.
                event_row(ui, &events[events.len() - 1 - i], &state.networks, i > 0, clock, p);
            }
        });
}

/// What a crossed limit, or the end of one, means in full: for a row's tooltip and
/// for notifications.
pub fn limit_detail(kind: &EventKind) -> Option<String> {
    Some(match *kind {
        EventKind::Loss { lost, pings, limit_pct } => format!(
            "{lost} of the last {pings} pings got no reply. The limit is {}.",
            format::percent(limit_pct)
        ),
        EventKind::LossEnded { limit_pct, pings } => {
            format!(
                "Loss has stayed under {} for {pings} pings.",
                format::percent(limit_pct)
            )
        }
        EventKind::Slow {
            avg_ms,
            pings,
            limit_ms,
        } => format!(
            "Replies averaged {} over the last {pings} pings. The limit is {}.",
            format::ms_unit(avg_ms),
            format::ms_unit(limit_ms)
        ),
        EventKind::SlowEnded { limit_ms, pings } => format!(
            "Replies have averaged {} or less for {pings} pings.",
            format::ms_unit(limit_ms)
        ),
        _ => return None,
    })
}

/// The mark at the start of a row.
enum Mark {
    Status(HostStatus),
    Switch,
    Limit,
}

fn event_row(ui: &mut Ui, event: &Event, networks: &Networks, separator: bool, clock: &Clock, p: &Palette) {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
    let (mark, message) = match &event.kind {
        EventKind::Down => (Mark::Status(HostStatus::Down), "Stopped answering".to_string()),
        EventKind::Up { outage_secs } => (
            Mark::Status(HostStatus::Up),
            format!("Answering again after {}", format::duration(*outage_secs)),
        ),
        EventKind::ResolveFailed(error) => (
            Mark::Status(HostStatus::ResolveFailed),
            format!("Can't resolve: {error}"),
        ),
        EventKind::ExitChanged { from, to } => (
            Mark::Switch,
            format!("Now leaves via {} (was {})", networks.name(*to), networks.name(*from)),
        ),
        EventKind::Loss { lost, pings, .. } => (
            Mark::Limit,
            format!("Losing pings: {lost} of the last {pings} got no reply"),
        ),
        EventKind::LossEnded { limit_pct, .. } => (
            Mark::Status(HostStatus::Up),
            format!("Loss back under {}", format::percent(*limit_pct)),
        ),
        EventKind::Slow { avg_ms, pings, .. } => (
            Mark::Limit,
            format!("Slow: {} on average over {pings} pings", format::ms_unit(*avg_ms)),
        ),
        EventKind::SlowEnded { limit_ms, .. } => (
            Mark::Status(HostStatus::Up),
            format!("Round trips back to {} or less", format::ms_unit(*limit_ms)),
        ),
    };
    // The row is painted rather than laid out, so tell screen readers what it says.
    let time = clock.day_time(event.at);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::Role::Label,
            true,
            format!("{}, {time}: {message}", event.host_name),
        )
    });
    if let EventKind::ExitChanged { from, to } = event.kind {
        response.on_hover_text(format!(
            "Traffic to this host left through {} (AS{from}) and now leaves through {} (AS{to}).",
            networks.name(from),
            networks.name(to)
        ));
    } else if let Some(detail) = limit_detail(&event.kind) {
        response.on_hover_text(detail);
    }

    let painter = ui.painter();
    if separator {
        painter.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, p.border));
    }
    let top = rect.top() + 16.0;
    let bottom = rect.top() + 35.0;
    let text_left = rect.left() + 20.0;
    let icon = pos2(rect.left() + 6.0, top);
    match mark {
        Mark::Status(status) => theme::paint_status_icon(painter, icon, 11.0, status, p),
        Mark::Switch => theme::paint_switch_icon(painter, icon, 11.0, p),
        Mark::Limit => theme::paint_limit_icon(painter, icon, 11.0, p),
    }

    let time = painter.layout_no_wrap(time, theme::font(12.0), p.text_muted);
    let time_width = time.size().x;
    painter.galley(
        pos2(rect.right() - time_width, top - time.size().y / 2.0),
        time,
        p.text_muted,
    );
    let name_width = (rect.right() - time_width - 10.0 - text_left).max(20.0);
    let name = truncated(
        painter,
        &event.host_name,
        theme::font_semibold(13.5),
        p.text,
        name_width,
    );
    painter.galley(pos2(text_left, top - name.size().y / 2.0), name, p.text);
    let message = truncated(
        painter,
        &message,
        theme::font(13.0),
        p.text_secondary,
        rect.right() - text_left,
    );
    painter.galley(
        pos2(text_left, bottom - message.size().y / 2.0),
        message,
        p.text_secondary,
    );
}

/// One line of text, cut with an ellipsis to fit `max_width`.
fn truncated(painter: &Painter, text: &str, font: FontId, color: Color32, max_width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = TextWrapping::truncate_at_width(max_width);
    painter.layout_job(job)
}
