//! Colors, fonts and widget styling for the light and dark themes.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Painter, Pos2, Shadow, Shape,
    Stroke, TextStyle, Theme, ThemePreference, Visuals, pos2, vec2,
};
use ferrisping_core::HostStatus;
use serde::{Deserialize, Serialize};

/// The theme the user picked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "system" | "auto" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "Match Windows",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn preference(self) -> ThemePreference {
        match self {
            Self::System => ThemePreference::System,
            Self::Light => ThemePreference::Light,
            Self::Dark => ThemePreference::Dark,
        }
    }
}

/// Color tokens for one theme. The status colors are reserved for host state
/// and always appear next to an icon and a label.
pub struct Palette {
    pub page: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub hover: Color32,
    pub active: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub grid: Color32,
    pub axis: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    /// The jitter strip's line, under the round trips' accent line. Both themes use
    /// the same magenta, which a color-blind reader can still tell from the accent.
    pub jitter: Color32,
    pub good: Color32,
    pub warning: Color32,
    pub serious: Color32,
    pub critical: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub const DARK: Palette = Palette {
    page: hex(0x0d0d0d),
    surface: hex(0x1a1a19),
    raised: hex(0x242423),
    hover: hex(0x2c2c2a),
    active: hex(0x343432),
    border: hex(0x313130),
    border_strong: hex(0x45453f),
    text: hex(0xf0efec),
    text_secondary: hex(0xc3c2b7),
    text_muted: hex(0x898781),
    grid: hex(0x2c2c2a),
    axis: hex(0x383835),
    accent: hex(0x3987e5),
    on_accent: hex(0xffffff),
    jitter: hex(0xd55181),
    good: hex(0x0ca30c),
    warning: hex(0xfab219),
    serious: hex(0xec835a),
    critical: hex(0xd03b3b),
};

pub const LIGHT: Palette = Palette {
    page: hex(0xf3f3f1),
    surface: hex(0xfcfcfb),
    raised: hex(0xf1f1ef),
    hover: hex(0xe8e8e5),
    active: hex(0xdfdfdb),
    border: hex(0xe4e4e3),
    border_strong: hex(0xcfcfca),
    text: hex(0x0b0b0b),
    text_secondary: hex(0x52514e),
    text_muted: hex(0x898781),
    grid: hex(0xe1e0d9),
    axis: hex(0xc3c2b7),
    accent: hex(0x2a78d6),
    on_accent: hex(0xffffff),
    jitter: hex(0xd55181),
    good: hex(0x0ca30c),
    warning: hex(0xfab219),
    serious: hex(0xec835a),
    critical: hex(0xd03b3b),
};

pub fn palette(dark_mode: bool) -> &'static Palette {
    if dark_mode { &DARK } else { &LIGHT }
}

/// Font family for emphasized text (Segoe UI Semibold when available).
pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn font_semibold(size: f32) -> FontId {
    FontId::new(size, semibold())
}

static HAS_ICONS: AtomicBool = AtomicBool::new(false);

/// A Segoe Fluent Icons glyph, or the fallback text if the icon font isn't installed.
pub fn icon(glyph: char, fallback: &'static str) -> String {
    if HAS_ICONS.load(Ordering::Relaxed) {
        glyph.to_string()
    } else {
        fallback.to_string()
    }
}

pub const ICON_ADD: char = '\u{E710}';
pub const ICON_PLAY: char = '\u{E768}';
pub const ICON_PAUSE: char = '\u{E769}';
pub const ICON_MORE: char = '\u{E712}';
pub const ICON_CLOSE: char = '\u{E711}';

/// Load fonts, install both themes' visuals and apply the chosen theme.
pub fn install(ctx: &egui::Context, choice: ThemeChoice) {
    install_fonts(ctx);
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, true));
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, false));
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, font(12.0)),
            (TextStyle::Body, font(14.0)),
            (TextStyle::Button, font(14.0)),
            (TextStyle::Heading, font_semibold(18.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        let spacing = &mut style.spacing;
        spacing.item_spacing = vec2(8.0, 6.0);
        spacing.button_padding = vec2(10.0, 4.0);
        spacing.interact_size.y = 28.0;
        spacing.menu_margin = Margin::same(6);
        spacing.window_margin = Margin::same(12);
        spacing.tooltip_width = 280.0;
    });
    ctx.set_theme(choice.preference());
}

fn install_fonts(ctx: &egui::Context) {
    let dir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    let load = |file: &str| std::fs::read(dir.join(file)).ok();

    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: Vec<u8>| {
        fonts
            .font_data
            .insert(name.to_string(), Arc::new(FontData::from_owned(bytes)));
    };

    // Text: Segoe UI. For characters it lacks, or if it's missing, eframe's system font
    // provider finds a font through Windows' own font fallback.
    let mut proportional = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(bytes) = load("segoeui.ttf") {
        add(&mut fonts, "segoe-ui", bytes);
        proportional.insert(0, "segoe-ui".into());
    }
    // Icons live in the private-use area, so they can sit right after the text font.
    if let Some(bytes) = load("SegoeIcons.ttf").or_else(|| load("segmdl2.ttf")) {
        add(&mut fonts, "segoe-icons", bytes);
        proportional.insert(1.min(proportional.len()), "segoe-icons".into());
        HAS_ICONS.store(true, Ordering::Relaxed);
    }
    let mut semibold_family = proportional.clone();
    if let Some(bytes) = load("seguisb.ttf") {
        add(&mut fonts, "segoe-ui-semibold", bytes);
        semibold_family.retain(|name| name != "segoe-ui");
        semibold_family.insert(0, "segoe-ui-semibold".into());
    }
    let mut monospace = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    if let Some(bytes) = load("consola.ttf") {
        add(&mut fonts, "consolas", bytes);
        monospace.insert(0, "consolas".into());
    }

    fonts.families.insert(FontFamily::Proportional, proportional);
    fonts.families.insert(semibold(), semibold_family);
    fonts.families.insert(FontFamily::Monospace, monospace);
    ctx.set_fonts(fonts);
}

fn visuals(p: &Palette, dark: bool) -> Visuals {
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    let radius = CornerRadius::same(6);

    v.panel_fill = p.page;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 120 } else { 30 }),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 100 } else { 26 }),
    };
    v.faint_bg_color = p.raised;
    v.extreme_bg_color = p.surface;
    v.text_edit_bg_color = Some(p.page);
    v.code_bg_color = p.raised;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.critical;
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);
    v.indent_has_left_vline = false;
    v.striped = false;

    let widgets = &mut v.widgets;
    widgets.noninteractive.bg_fill = p.surface;
    widgets.noninteractive.weak_bg_fill = p.surface;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    widgets.noninteractive.corner_radius = radius;

    widgets.inactive.bg_fill = p.raised;
    widgets.inactive.weak_bg_fill = p.raised;
    widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
    widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    widgets.inactive.corner_radius = radius;
    widgets.inactive.expansion = 0.0;

    widgets.hovered.bg_fill = p.hover;
    widgets.hovered.weak_bg_fill = p.hover;
    widgets.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);
    widgets.hovered.fg_stroke = Stroke::new(1.5, p.text);
    widgets.hovered.corner_radius = radius;
    widgets.hovered.expansion = 0.0;

    widgets.active.bg_fill = p.active;
    widgets.active.weak_bg_fill = p.active;
    widgets.active.bg_stroke = Stroke::new(1.0, p.border_strong);
    widgets.active.fg_stroke = Stroke::new(1.5, p.text);
    widgets.active.corner_radius = radius;
    widgets.active.expansion = 0.0;

    widgets.open = widgets.active;
    v
}

/// Status color for a host. Never used for anything but host state.
pub fn status_color(p: &Palette, status: HostStatus) -> Color32 {
    match status {
        HostStatus::Up => p.good,
        HostStatus::Down => p.critical,
        HostStatus::ResolveFailed => p.serious,
        HostStatus::Resolving | HostStatus::Waiting => p.text_muted,
    }
}

/// The mark for traffic switching to another network: two arrows, one each way.
/// Accent colored, since the status colors are kept for host state.
pub fn paint_switch_icon(painter: &Painter, center: Pos2, size: f32, p: &Palette) {
    let r = size / 2.0;
    let stroke = Stroke::new(1.5, p.accent);
    for (y, dir) in [(center.y - r * 0.45, 1.0), (center.y + r * 0.45, -1.0)] {
        let tip = pos2(center.x + dir * r, y);
        painter.line_segment([pos2(center.x - dir * r, y), tip], stroke);
        for barb in [-1.0, 1.0] {
            painter.line_segment([tip, pos2(tip.x - dir * r * 0.5, y + barb * r * 0.4)], stroke);
        }
    }
}

/// The shape-coded status mark: a dot for up, a down triangle for down, a warning
/// triangle for name lookup trouble and a ring while waiting. The shapes differ so
/// state never depends on telling colors apart.
pub fn paint_status_icon(painter: &Painter, center: Pos2, size: f32, status: HostStatus, p: &Palette) {
    let r = size / 2.0;
    let color = status_color(p, status);
    match status {
        HostStatus::Up => {
            painter.circle_filled(center, r * 0.78, color);
        }
        HostStatus::Down => {
            let points = vec![
                pos2(center.x - r, center.y - r * 0.72),
                pos2(center.x + r, center.y - r * 0.72),
                pos2(center.x, center.y + r * 0.95),
            ];
            painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
        }
        HostStatus::ResolveFailed => {
            let points = vec![
                pos2(center.x, center.y - r * 0.95),
                pos2(center.x + r, center.y + r * 0.72),
                pos2(center.x - r, center.y + r * 0.72),
            ];
            painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
        }
        HostStatus::Resolving | HostStatus::Waiting => {
            painter.circle_stroke(center, r * 0.7, Stroke::new(1.5, color));
        }
    }
}
