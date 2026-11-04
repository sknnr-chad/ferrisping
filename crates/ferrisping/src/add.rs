//! Adding hosts: the box in the toolbar, and a panel under it with room for a list,
//! a host on each line, and the hosts added before. In a narrow window the toolbar
//! keeps only the Add button, which opens the panel.

use std::collections::HashSet;

use eframe::egui::{
    self, Align, Button, Id, Key, Layout, Margin, Popup, PopupCloseBehavior, Response, RichText, ScrollArea, TextEdit,
    Ui, vec2,
};
use ferrisping_core::{Family, Host, TargetSpec, entries, parse_targets};

use crate::app::accent_button;
use crate::theme::{self, Palette};

/// The toolbar's box at its narrowest and widest.
pub const BOX_MIN: f32 = 150.0;
pub const BOX_MAX: f32 = 300.0;
const PANEL_WIDTH: f32 = 340.0;
/// The tallest the list of hosts added before gets before it scrolls.
const RECENT_HEIGHT: f32 = 220.0;
/// Most hosts kept in the list of hosts added before.
const RECENT_MAX: usize = 50;

/// What's typed in the add boxes, and the hosts added before.
#[derive(Default)]
pub struct Adding {
    /// The toolbar's box.
    pub text: String,
    /// The panel's box, which takes a list with a host on each line.
    pub list: String,
    /// Hosts added or removed before, as typed and newest first, to add again.
    pub recent: Vec<String>,
    /// Put the cursor in the panel's box when it's next drawn.
    focus_list: bool,
}

impl Adding {
    /// Empty boxes, and the hosts added before as saved.
    pub fn new(recent: Vec<String>) -> Self {
        Self {
            recent,
            ..Self::default()
        }
    }

    /// Put what was just added or removed at the top of the hosts added before. An
    /// entry for the same host moves up, with its newest name, rather than show twice.
    pub fn remember(&mut self, input: &str) {
        let typed: Vec<String> = entries(input)
            .map(|entry| entry.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        for entry in typed.into_iter().rev() {
            let host = host_word(&entry);
            self.recent.retain(|old| host_word(old) != host);
            self.recent.insert(0, entry);
        }
        self.recent.truncate(RECENT_MAX);
    }
}

/// The host an entry names, with any `ipv4:` or `ipv6:` prefix, in lower case.
fn host_word(entry: &str) -> String {
    entry.split_whitespace().next().unwrap_or_default().to_ascii_lowercase()
}

/// How the add box fits in the toolbar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Form {
    /// The box, this wide, with its buttons.
    Box(f32),
    /// Only the Add button, which opens the panel.
    Button,
}

#[derive(Default)]
pub struct AddOutput {
    /// What to add, as typed.
    pub inputs: Vec<String>,
    /// The Add button's width, and the ⌄ button's beside the box, for fitting the toolbar.
    pub add_width: f32,
    pub more_width: Option<f32>,
}

/// The add box with its buttons, or just the Add button. The ⌄ beside the box, or the
/// Add button on its own, opens the panel.
pub fn show(ui: &mut Ui, adding: &mut Adding, p: &Palette, form: Form, hosts: &[Host]) -> AddOutput {
    let mut out = AddOutput::default();
    let label = format!("{}  Add", theme::icon(theme::ICON_ADD, "+"));
    let opener = match form {
        Form::Box(width) => {
            let id = Id::unique("add box");
            one_line_paste(ui, id);
            let response = ui.add(
                TextEdit::singleline(&mut adding.text)
                    .id(id)
                    .hint_text("Add hosts: names or IPs, comma-separated")
                    .desired_width(width)
                    .margin(Margin::symmetric(8, 5)),
            );
            let entered = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let more = ui
                .button(theme::icon(theme::ICON_CHEVRON_DOWN, "⏷"))
                .on_hover_text("Paste a list, or add a host you had before")
                .accessible_name("More ways to add");
            let add = ui
                .add_enabled(!adding.text.trim().is_empty(), accent_button(p, &label))
                .accessible_name("Add");
            out.add_width = add.rect.width();
            out.more_width = Some(more.rect.width());
            if entered || add.clicked() {
                if !parse_targets(&adding.text).is_empty() {
                    out.inputs.push(std::mem::take(&mut adding.text));
                }
                if entered {
                    response.request_focus();
                }
            }
            more
        }
        Form::Button => {
            let add = ui
                .add(accent_button(p, &label))
                .on_hover_text("Type or paste hosts, or add one you had before")
                .accessible_name("Add hosts");
            out.add_width = add.rect.width();
            add
        }
    };
    panel(adding, p, &opener, hosts, &mut out);
    out
}

/// A list pasted into the toolbar's box, a host on each line, as one line with commas.
/// The box keeps to one line, so egui would join the lines with spaces, and every host
/// after the first would become part of the first one's name.
fn one_line_paste(ui: &Ui, id: Id) {
    if !ui.memory(|m| m.has_focus(id)) {
        return;
    }
    ui.ctx().input_mut(|input| {
        for event in &mut input.events {
            if let egui::Event::Paste(text) = event
                && text.contains(['\n', '\r'])
            {
                *text = one_line(text);
            }
        }
    });
}

/// `text`'s lines, trimmed, joined with commas, leaving out empty ones.
fn one_line(text: &str) -> String {
    text.split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The panel under `opener`: a box for a list, a host on each line, and the hosts added
/// before that aren't watched now.
fn panel(adding: &mut Adding, p: &Palette, opener: &Response, hosts: &[Host], out: &mut AddOutput) {
    if opener.clicked() {
        adding.focus_list = true;
    }
    Popup::menu(opener)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .width(PANEL_WIDTH)
        .show(|ui| {
            ui.label(
                RichText::new("Add hosts")
                    .font(theme::font_semibold(14.0))
                    .color(p.text),
            );
            let edit = ui.add(
                TextEdit::multiline(&mut adding.list)
                    .hint_text("Names or IP addresses, a host on each line or separated by commas")
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );
            if std::mem::take(&mut adding.focus_list) {
                edit.request_focus();
            }
            let shortcut = edit.has_focus() && ui.input(|i| i.modifiers.command && i.key_pressed(Key::Enter));
            let ready = !parse_targets(&adding.list).is_empty();
            let row = vec2(ui.available_width(), ui.spacing().interact_size.y);
            let clicked = ui
                .allocate_ui_with_layout(row, Layout::right_to_left(Align::Center), |ui| {
                    ui.add_enabled(ready, accent_button(p, "Add"))
                        .on_hover_text("Or press Ctrl+Enter")
                        .accessible_name("Add the list")
                        .clicked()
                })
                .inner;
            if ready && (clicked || shortcut) {
                out.inputs.push(std::mem::take(&mut adding.list));
                ui.close();
            }
            recent(ui, adding, p, hosts, out);
        });
}

/// The hosts added before that aren't all watched now, to add again with a click.
fn recent(ui: &mut Ui, adding: &mut Adding, p: &Palette, hosts: &[Host], out: &mut AddOutput) {
    let watched: HashSet<(Family, String)> = hosts.iter().map(|h| key(&h.spec)).collect();
    let shown: Vec<String> = adding
        .recent
        .iter()
        .filter(|entry| parse_targets(entry).iter().any(|spec| !watched.contains(&key(spec))))
        .cloned()
        .collect();
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Added before").size(12.0).color(p.text_muted));
        if !shown.is_empty() {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button("Clear")
                    .on_hover_text("Forget the hosts added before")
                    .clicked()
                {
                    adding.recent.clear();
                }
            });
        }
    });
    if shown.is_empty() {
        ui.label(
            RichText::new("Hosts you add or remove show here, to add again with a click.")
                .size(12.5)
                .color(p.text_secondary),
        );
        return;
    }
    // A popup is as tall as it was last frame, so ask for the list's height: a scroll
    // area would shrink to fit instead, and the popup would never grow.
    let row = ui.spacing().interact_size.y + ui.spacing().item_spacing.y;
    let height = (shown.len() as f32 * row).min(RECENT_HEIGHT);
    ScrollArea::vertical()
        .min_scrolled_height(height)
        .max_height(RECENT_HEIGHT)
        .show(ui, |ui| {
            for entry in shown {
                if ui
                    .add(Button::new(&entry).frame_when_inactive(false))
                    .on_hover_text("Watch it again")
                    .clicked()
                {
                    out.inputs.push(entry);
                }
            }
        });
}

/// A host as [`TargetSpec::same_host`] tells them apart.
fn key(spec: &TargetSpec) -> (Family, String) {
    (spec.family, spec.host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_list_becomes_one_line_of_entries() {
        assert_eq!(
            one_line("10.0.0.1\r\n10.0.0.2  \n\n  printer Front desk\n"),
            "10.0.0.1, 10.0.0.2, printer Front desk"
        );
        let specs = parse_targets(&one_line("192.0.2.1 Gateway\n192.0.2.2"));
        let hosts: Vec<_> = specs.iter().map(|s| (s.host.as_str(), s.label.as_deref())).collect();
        assert_eq!(hosts, [("192.0.2.1", Some("Gateway")), ("192.0.2.2", None)]);
    }

    #[test]
    fn the_newest_entries_come_first_and_a_host_shows_once() {
        let mut adding = Adding::default();
        adding.remember("192.0.2.1, 192.0.2.2  Printer");
        assert_eq!(adding.recent, ["192.0.2.1", "192.0.2.2 Printer"]);
        // The same host under a new name moves up, and its case doesn't matter.
        adding.remember("example.com; 192.0.2.2 Front desk");
        adding.remember("EXAMPLE.com");
        assert_eq!(adding.recent, ["EXAMPLE.com", "192.0.2.2 Front desk", "192.0.2.1"]);
        // The same address over another family is another host.
        adding.remember("ipv6:example.com");
        assert_eq!(adding.recent.len(), 4);
    }

    #[test]
    fn the_list_keeps_the_newest_entries() {
        let mut adding = Adding::default();
        for i in 0..RECENT_MAX + 5 {
            adding.remember(&format!("host{i}"));
        }
        assert_eq!(adding.recent.len(), RECENT_MAX);
        assert_eq!(adding.recent[0], format!("host{}", RECENT_MAX + 4));
    }
}
