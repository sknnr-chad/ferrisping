//! The event log: when each host stopped answering and when it came back, and when
//! its traffic started leaving through another network (ISP).

use std::sync::Arc;

use eframe::egui::text::{LayoutJob, TextWrapping};
use eframe::egui::{self, Color32, FontId, Galley, Painter, RichText, Sense, Stroke, Ui, pos2, vec2};
use ferrisping_core::{Event, EventKind, HostStatus, Networks, State};

use crate::format::{self, Clock};
use crate::theme::{self, Palette};

const ROW_HEIGHT: f32 = 50.0;
/// Room between a host's name and its address.
const ADDRESS_GAP: f32 = 6.0;

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

fn event_row(ui: &mut Ui, event: &Event, networks: &Networks, separator: bool, clock: &Clock, p: &Palette) {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
    // A host status, or None for the switch mark.
    let (status, message) = match &event.kind {
        EventKind::Down => (Some(HostStatus::Down), "Stopped answering".to_string()),
        EventKind::Up { outage_secs } => (
            Some(HostStatus::Up),
            format!("Answering again after {}", format::duration(*outage_secs)),
        ),
        EventKind::ResolveFailed(error) => (Some(HostStatus::ResolveFailed), format!("Can't resolve: {error}")),
        EventKind::ExitChanged { from, to } => (
            None,
            format!("Now leaves via {} (was {})", networks.name(*to), networks.name(*from)),
        ),
    };
    // The host as it was when this was logged: its address too, when that's not its name.
    let address = Some(&event.host_address).filter(|address| **address != event.host_name);
    // The row is painted rather than laid out, so tell screen readers what it says.
    let time = clock.day_time(event.at);
    response.widget_info(|| {
        let host = match address {
            Some(address) => format!("{} ({address})", event.host_name),
            None => event.host_name.clone(),
        };
        egui::WidgetInfo::labeled(egui::Role::Label, true, format!("{host}, {time}: {message}"))
    });
    if let EventKind::ExitChanged { from, to } = event.kind {
        response.on_hover_text(format!(
            "Traffic to this host left through {} (AS{from}) and now leaves through {} (AS{to}).",
            networks.name(from),
            networks.name(to)
        ));
    }

    let painter = ui.painter();
    if separator {
        painter.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, p.border));
    }
    let top = rect.top() + 16.0;
    let bottom = rect.top() + 35.0;
    let text_left = rect.left() + 20.0;
    let icon = pos2(rect.left() + 6.0, top);
    match status {
        Some(status) => theme::paint_status_icon(painter, icon, 11.0, status, p),
        None => theme::paint_switch_icon(painter, icon, 11.0, p),
    }

    let time = painter.layout_no_wrap(time, theme::font(12.0), p.text_muted);
    let time_width = time.size().x;
    painter.galley(
        pos2(rect.right() - time_width, top - time.size().y / 2.0),
        time,
        p.text_muted,
    );
    // The name is cut short to fit the address, which shows whole or not at all.
    let room = (rect.right() - time_width - 10.0 - text_left).max(20.0);
    let name_in = |width: f32| truncated(painter, &event.host_name, theme::font_semibold(13.5), p.text, width);
    let address = address.map(|address| painter.layout_no_wrap(address.clone(), theme::font(12.5), p.text_muted));
    let (name, address) = match address {
        Some(address) => {
            let width = room - ADDRESS_GAP - address.size().x;
            let name = name_in(width.max(0.0));
            if !name.elided || width >= theme::NAME_MIN_WIDTH {
                (name, Some(address))
            } else {
                (name_in(room), None)
            }
        }
        None => (name_in(room), None),
    };
    let address_left = text_left + name.size().x + ADDRESS_GAP;
    painter.galley(pos2(text_left, top - name.size().y / 2.0), name, p.text);
    if let Some(address) = address {
        painter.galley(pos2(address_left, top - address.size().y / 2.0), address, p.text_muted);
    }
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
