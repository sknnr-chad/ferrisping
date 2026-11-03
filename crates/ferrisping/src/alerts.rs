//! What raises an alert, and the two dialogs that set it: Alerts… under More
//! options for every host, and Alerts… on a lane's menu for that host alone.

use std::time::Duration;

use eframe::egui::{self, Align, Checkbox, DragValue, Layout, RichText, Ui};
use ferrisping_core::{AlertSettings, Host, HostAlerts, HostId, Limits};
use serde::{Deserialize, Serialize};

use crate::app::accent_button;
use crate::format;
use crate::theme::{self, Palette};

/// Limits on loss and round trips as the dialogs show them. A number stays while
/// its box is unticked, so ticking it again brings it back.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitChoices {
    pub pings: u32,
    pub loss: bool,
    pub loss_pct: f32,
    pub slow: bool,
    pub slow_ms: f32,
}

impl Default for LimitChoices {
    fn default() -> Self {
        // Off until they're wanted, since no one limit suits both a host down the
        // hall and one across the world. The numbers are suggestions.
        Self {
            pings: 30,
            loss: false,
            loss_pct: 10.0,
            slow: false,
            slow_ms: 200.0,
        }
    }
}

impl LimitChoices {
    pub fn limits(&self) -> Limits {
        Limits {
            pings: self.pings,
            loss_pct: self.loss.then_some(self.loss_pct),
            slow_ms: self.slow.then_some(self.slow_ms),
        }
    }

    /// The choices for `limits`, with `others`' numbers for the limits that are off.
    fn from_limits(limits: Limits, others: &LimitChoices) -> Self {
        Self {
            pings: limits.pings,
            loss: limits.loss_pct.is_some(),
            loss_pct: limits.loss_pct.unwrap_or(others.loss_pct),
            slow: limits.slow_ms.is_some(),
            slow_ms: limits.slow_ms.unwrap_or(others.slow_ms),
        }
    }
}

/// Which events are alerts for hosts without alerts of their own, and how alerts show.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertChoices {
    pub outages: bool,
    pub switches: bool,
    pub limits: LimitChoices,
    /// Alerts show as Windows notifications.
    pub notifications: bool,
    /// And play Windows' notification sound.
    pub sound: bool,
}

impl Default for AlertChoices {
    fn default() -> Self {
        Self {
            outages: true,
            switches: true,
            limits: LimitChoices::default(),
            notifications: true,
            sound: true,
        }
    }
}

impl AlertChoices {
    pub fn settings(&self) -> AlertSettings {
        AlertSettings {
            outages: self.outages,
            switches: self.switches,
            limits: self.limits.limits(),
        }
    }

    /// `Now: outages, network switches and loss of 10% or more.`
    fn summary(&self) -> String {
        let limits = &self.limits;
        let mut parts = Vec::new();
        if self.outages {
            parts.push("outages".to_string());
        }
        if self.switches {
            parts.push("network switches".to_string());
        }
        if limits.loss {
            parts.push(format!("loss of {} or more", format::percent(limits.loss_pct)));
        }
        if limits.slow {
            parts.push(format!("round trips over {}", format::ms_unit(limits.slow_ms)));
        }
        match parts.as_slice() {
            [] => "Now: no alerts.".to_string(),
            [one] => format!("Now: {one}."),
            [first @ .., last] => format!("Now: {} and {last}.", first.join(", ")),
        }
    }
}

/// A host's own alerts, as saved.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum HostChoice {
    Off,
    Own(LimitChoices),
}

impl HostChoice {
    pub fn alerts(self) -> HostAlerts {
        match self {
            Self::Off => HostAlerts::Off,
            Self::Own(limits) => HostAlerts::Own(limits.limits()),
        }
    }

    /// What to save for a host's alerts, if they're its own.
    pub fn of(alerts: HostAlerts, others: &LimitChoices) -> Option<Self> {
        match alerts {
            HostAlerts::Usual => None,
            HostAlerts::Off => Some(Self::Off),
            HostAlerts::Own(limits) => Some(Self::Own(LimitChoices::from_limits(limits, others))),
        }
    }
}

/// How a dialog ended.
pub enum Outcome<T> {
    Saved(T),
    Cancelled,
}

/// The Alerts dialog, while it's open: the choices as they're being changed.
pub struct AlertsDialog {
    choices: AlertChoices,
}

impl AlertsDialog {
    pub fn new(choices: AlertChoices) -> Self {
        Self { choices }
    }

    /// Draw it. `lookups`: switches to another network are being looked for.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        p: &Palette,
        interval: Duration,
        lookups: bool,
    ) -> Option<Outcome<AlertChoices>> {
        let choices = &mut self.choices;
        let mut outcome = None;
        let modal = egui::Modal::new(egui::Id::unique("alerts")).show(ctx, |ui| {
            ui.set_width(440.0);
            title(ui, p, "Alerts");
            ui.label(
                RichText::new("What FerrisPing tells you about with a Windows notification.").color(p.text_secondary),
            );
            ui.add_space(8.0);
            ui.checkbox(
                &mut choices.outages,
                "A host stops answering, and when it answers again",
            )
            .on_hover_text(
                "A host that has never answered, like an unused address in a subnet, is only logged in Events",
            );
            ui.add_enabled(
                lookups,
                Checkbox::new(
                    &mut choices.switches,
                    "Traffic to a host switches to another network (ISP)",
                ),
            )
            .on_disabled_hover_text(
                "Switches are found only while routes are checked. Turn on \"Check routes and look up \
                 networks\" under More options.",
            );
            limit_rows(ui, &mut choices.limits, interval);
            ui.add_space(4.0);
            ui.label(
                RichText::new("A host can have limits of its own: right-click it, then Alerts…")
                    .size(12.5)
                    .color(p.text_muted),
            );
            ui.separator();
            ui.checkbox(&mut choices.notifications, "Show notifications")
                .on_hover_text("From FerrisPing's icon in the tray, by the clock");
            ui.add_enabled(choices.notifications, Checkbox::new(&mut choices.sound, "Play a sound"))
                .on_hover_text("Windows' notification sound");
            ui.label(
                RichText::new("Alerts are logged in Events either way.")
                    .size(12.5)
                    .color(p.text_muted),
            );
            ui.add_space(10.0);
            outcome = buttons(ui, p);
        });
        match outcome {
            Some(true) => Some(Outcome::Saved(*choices)),
            Some(false) => Some(Outcome::Cancelled),
            None => modal.should_close().then_some(Outcome::Cancelled),
        }
    }
}

/// Which alerts a host has, as picked in its dialog.
#[derive(Clone, Copy, PartialEq)]
enum Pick {
    Usual,
    Off,
    Own,
}

/// A host's Alerts dialog, while it's open.
pub struct HostAlertsDialog {
    pub host: HostId,
    name: String,
    /// The host's address, when it isn't its name.
    address: Option<String>,
    pick: Pick,
    /// Its own limits, starting from the usual ones.
    limits: LimitChoices,
}

impl HostAlertsDialog {
    pub fn new(host: &Host, usual: &LimitChoices) -> Self {
        let (pick, limits) = match host.alerts {
            HostAlerts::Usual => (Pick::Usual, *usual),
            HostAlerts::Off => (Pick::Off, *usual),
            HostAlerts::Own(limits) => (Pick::Own, LimitChoices::from_limits(limits, usual)),
        };
        Self {
            host: host.id,
            name: host.name().to_string(),
            address: host
                .addr
                .map(|addr| addr.to_string())
                .filter(|addr| *addr != host.name()),
            pick,
            limits,
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        p: &Palette,
        interval: Duration,
        usual: &AlertChoices,
    ) -> Option<Outcome<HostAlerts>> {
        let mut outcome = None;
        let modal = egui::Modal::new(egui::Id::unique("host-alerts")).show(ctx, |ui| {
            ui.set_width(420.0);
            title(ui, p, &format!("Alerts for {}", self.name));
            if let Some(address) = &self.address {
                ui.label(RichText::new(address).size(12.5).color(p.text_muted));
            }
            ui.add_space(8.0);
            ui.radio_value(&mut self.pick, Pick::Usual, "The same as other hosts");
            note(ui, p, &usual.summary());
            ui.radio_value(&mut self.pick, Pick::Off, "None for this host");
            note(ui, p, "Its outages are still logged in Events.");
            ui.radio_value(&mut self.pick, Pick::Own, "Limits of its own");
            ui.add_enabled_ui(self.pick == Pick::Own, |ui| {
                ui.indent("own-limits", |ui| {
                    limit_rows(ui, &mut self.limits, interval);
                    ui.label(
                        RichText::new("Its outages and switches are alerts as they are for other hosts.")
                            .size(12.5)
                            .color(p.text_muted),
                    );
                });
            });
            ui.add_space(10.0);
            outcome = buttons(ui, p);
        });
        let alerts = match self.pick {
            Pick::Usual => HostAlerts::Usual,
            Pick::Off => HostAlerts::Off,
            Pick::Own => HostAlerts::Own(self.limits.limits()),
        };
        match outcome {
            Some(true) => Some(Outcome::Saved(alerts)),
            Some(false) => Some(Outcome::Cancelled),
            None => modal.should_close().then_some(Outcome::Cancelled),
        }
    }
}

fn title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.label(RichText::new(text).font(theme::font_semibold(17.0)).color(p.text));
}

/// A line under a choice, saying what it means.
fn note(ui: &mut Ui, p: &Palette, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(ui.spacing().icon_width + ui.spacing().icon_spacing);
        ui.label(RichText::new(text).size(12.5).color(p.text_muted));
    });
}

/// The rows for loss and round trips, and for how many pings they're judged over.
fn limit_rows(ui: &mut Ui, limits: &mut LimitChoices, interval: Duration) {
    ui.horizontal(|ui| {
        ui.checkbox(&mut limits.loss, "Loss reaches");
        let value = DragValue::new(&mut limits.loss_pct)
            .range(1.0..=100.0)
            .speed(0.5)
            .max_decimals(0)
            .suffix("%");
        ui.add_enabled(limits.loss, value).accessible_name("Loss limit");
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut limits.slow, "Round trips average more than");
        let value = DragValue::new(&mut limits.slow_ms)
            .range(1.0..=10_000.0)
            .speed(5.0)
            .max_decimals(0)
            .suffix(" ms");
        ui.add_enabled(limits.slow, value).accessible_name("Round-trip limit");
    });
    ui.add_enabled_ui(limits.loss || limits.slow, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().icon_width + ui.spacing().icon_spacing);
            ui.label("Over the last");
            ui.add(DragValue::new(&mut limits.pings).range(2..=3600))
                .accessible_name("Pings judged");
            let span = format::duration(f64::from(limits.pings) * interval.as_secs_f64());
            ui.label(format!("pings ({span} at the current interval)"));
        });
    });
}

/// Save and Cancel, on the right. Some(true) once saved.
fn buttons(ui: &mut Ui, p: &Palette) -> Option<bool> {
    let mut pressed = None;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if ui.add(accent_button(p, "Save")).clicked() {
            pressed = Some(true);
        }
        if ui.button("Cancel").clicked() {
            pressed = Some(false);
        }
    });
    pressed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unticked_limits_keep_their_numbers_but_are_off() {
        let choices = LimitChoices {
            loss: false,
            loss_pct: 5.0,
            slow: true,
            slow_ms: 80.0,
            pings: 60,
        };
        assert_eq!(
            choices.limits(),
            Limits {
                pings: 60,
                loss_pct: None,
                slow_ms: Some(80.0)
            }
        );
        // Read back from the engine, an unticked number comes from the usual limits.
        let usual = LimitChoices::default();
        assert_eq!(
            LimitChoices::from_limits(choices.limits(), &usual),
            LimitChoices {
                loss_pct: usual.loss_pct,
                ..choices
            }
        );
    }

    #[test]
    fn the_summary_lists_what_raises_an_alert() {
        let mut choices = AlertChoices::default();
        assert_eq!(choices.summary(), "Now: outages and network switches.");
        choices.limits.loss = true;
        assert_eq!(
            choices.summary(),
            "Now: outages, network switches and loss of 10% or more."
        );
        choices = AlertChoices {
            outages: false,
            switches: false,
            ..AlertChoices::default()
        };
        assert_eq!(choices.summary(), "Now: no alerts.");
    }
}
