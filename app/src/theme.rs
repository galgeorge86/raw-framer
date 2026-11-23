#![allow(dead_code)] // icon set and helpers kept for upcoming UI
//! Darkroom Lightbox design tokens (see .claude/DESIGN.md). Achromatic shell; the only hue is
//! the red "hero" dot. Every colour used by the UI comes from here.

use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId};
use std::sync::Arc;

// ---- palette (all neutral greys: R = G = B within ±2 to stay achromatic)
pub const VIEWER: Color32 = Color32::from_rgb(0x09, 0x09, 0x0a);
pub const ROOM: Color32 = Color32::from_rgb(0x0d, 0x0d, 0x0e);
pub const TRAY: Color32 = Color32::from_rgb(0x0f, 0x0f, 0x10);
pub const WELL: Color32 = Color32::from_rgb(0x15, 0x15, 0x17);
pub const HOVER: Color32 = Color32::from_rgb(0x1b, 0x1b, 0x1d);
pub const HAIRLINE: Color32 = Color32::from_rgb(0x1d, 0x1d, 0x1f);
pub const CONTROL: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x2c);
pub const CONTROL_HI: Color32 = Color32::from_rgb(0x36, 0x36, 0x38);
pub const MUTED: Color32 = Color32::from_rgb(0x7c, 0x7c, 0x7e);
pub const SECONDARY: Color32 = Color32::from_rgb(0x9a, 0x9a, 0x9c);
pub const INK_DIM: Color32 = Color32::from_rgb(0xd6, 0xd6, 0xd4);
pub const INK: Color32 = Color32::from_rgb(0xee, 0xee, 0xec);
pub const HERO: Color32 = Color32::from_rgb(0xd2, 0x3f, 0x2c);

// ---- spacing (4-pt scale)
pub const S1: f32 = 4.0;
pub const S2: f32 = 8.0;
pub const S3: f32 = 12.0;
pub const S4: f32 = 16.0;
pub const S5: f32 = 24.0;

// ---- layout
pub const TOP_BAR_H: f32 = 36.0;
pub const TRANSPORT_H: f32 = 60.0;
pub const TRAY_H: f32 = 132.0;
pub const SIDE_W: f32 = 320.0;
pub const RADIUS: u8 = 6;

// ---- type
pub const FONT_IMPRINT: &str = "imprint";
pub const FONT_STRONG: &str = "strong";

pub fn ui(size: f32) -> FontId {
    FontId::proportional(size)
}
pub fn strong(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FONT_STRONG.into()))
}
pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}
pub fn imprint(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FONT_IMPRINT.into()))
}

/// Phosphor icon codepoints (MIT, vendored font).
pub mod icon {
    pub const PLAY: &str = "\u{E3D0}";
    pub const PAUSE: &str = "\u{E39E}";
    pub const SKIP_BACK: &str = "\u{E5A4}";
    pub const SKIP_FORWARD: &str = "\u{E5A6}";
    pub const REWIND: &str = "\u{E6A8}";
    pub const FAST_FORWARD: &str = "\u{E6A6}";
    pub const CARET_LEFT: &str = "\u{E138}";
    pub const CARET_RIGHT: &str = "\u{E13A}";
    pub const CARET_DOWN: &str = "\u{E136}";
    pub const FOLDER_OPEN: &str = "\u{E256}";
    pub const FILM_STRIP: &str = "\u{E792}";
    pub const EXPORT: &str = "\u{EAF0}";
    pub const X: &str = "\u{E4F6}";
    pub const CHECK: &str = "\u{E182}";
    pub const WARNING: &str = "\u{E4E0}";
    pub const CUBE: &str = "\u{E1DA}";
    pub const CORNERS_OUT: &str = "\u{E1D0}";
    pub const SPINNER: &str = "\u{E66C}";
    pub const RESET: &str = "\u{E038}";
    pub const INFO: &str = "\u{E2CE}";
    pub const PLUS: &str = "\u{E3D4}";
    pub const IMAGE: &str = "\u{E2CC}";
    pub const SLIDERS: &str = "\u{E434}";
    pub const PIN: &str = "\u{E3E2}";
    pub const FLOPPY: &str = "\u{E248}";
    pub const COPY: &str = "\u{E1CA}";
}

fn load_system_font(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
    std::fs::read(dir.join("Fonts").join(name)).ok()
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut add = |name: &str, bytes: Vec<u8>| {
        fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_owned(bytes)));
    };
    add("jetbrains", include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").to_vec());
    add("barlow", include_bytes!("../assets/fonts/BarlowCondensed-SemiBold.ttf").to_vec());
    add("phosphor", include_bytes!("../assets/fonts/Phosphor.ttf").to_vec());
    let has_segoe = match load_system_font("segoeui.ttf") {
        Some(b) => {
            add("segoe", b);
            true
        }
        None => false,
    };
    let has_segoe_sb = match load_system_font("seguisb.ttf") {
        Some(b) => {
            add("segoe-sb", b);
            true
        }
        None => false,
    };

    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    if has_segoe {
        prop.insert(0, "segoe".into());
    }
    prop.push("phosphor".into());
    let monof = fonts.families.entry(FontFamily::Monospace).or_default();
    monof.insert(0, "jetbrains".into());
    monof.push("phosphor".into());

    let mut strong_fam = vec![];
    if has_segoe_sb {
        strong_fam.push("segoe-sb".to_string());
    }
    strong_fam.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
    fonts.families.insert(FontFamily::Name(FONT_STRONG.into()), strong_fam);
    let mut imp = vec!["barlow".to_string()];
    imp.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
    fonts.families.insert(FontFamily::Name(FONT_IMPRINT.into()), imp);
    ctx.set_fonts(fonts);
}

pub fn apply_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        use egui::{TextStyle::*, *};
        style.text_styles = [
            (Small, ui(11.0)),
            (Body, ui(13.0)),
            (Button, ui(13.0)),
            (Heading, strong(15.0)),
            (Monospace, mono(12.0)),
        ]
        .into();
        style.spacing.item_spacing = vec2(S2, 6.0);
        style.spacing.button_padding = vec2(10.0, 5.0);
        style.spacing.interact_size = vec2(24.0, 26.0);
        style.spacing.slider_width = 112.0;
        style.spacing.combo_width = 150.0;
        style.spacing.scroll = egui::style::ScrollStyle { floating: false, bar_width: 6.0, ..egui::style::ScrollStyle::thin() };
        // Emil: crisp, short UI motion; keyboard actions never animate.
        style.animation_time = 0.12;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = None;
        v.panel_fill = ROOM;
        v.window_fill = Color32::from_rgb(0x14, 0x14, 0x15);
        v.window_stroke = Stroke::new(1.0, HAIRLINE);
        v.window_corner_radius = CornerRadius::same(10);
        v.window_shadow = Shadow { offset: [0, 10], blur: 28, spread: 0, color: Color32::from_black_alpha(150) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(140) };
        v.menu_corner_radius = CornerRadius::same(8);
        v.extreme_bg_color = WELL;
        v.faint_bg_color = Color32::from_rgb(0x12, 0x12, 0x13);
        v.code_bg_color = WELL;
        v.hyperlink_color = INK;
        v.selection.bg_fill = Color32::from_rgb(0x3a, 0x3a, 0x3c);
        v.selection.stroke = Stroke::new(1.0, INK);
        v.text_cursor.stroke = Stroke::new(1.5, INK);
        v.slider_trailing_fill = true;
        v.handle_shape = style::HandleShape::Circle;
        v.striped = false;
        v.indent_has_left_vline = false;

        let r = CornerRadius::same(RADIUS);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = ROOM;
        w.noninteractive.weak_bg_fill = ROOM;
        w.noninteractive.bg_stroke = Stroke::new(1.0, HAIRLINE);
        w.noninteractive.fg_stroke = Stroke::new(1.0, SECONDARY);
        w.noninteractive.corner_radius = r;

        w.inactive.bg_fill = CONTROL;
        w.inactive.weak_bg_fill = WELL;
        w.inactive.bg_stroke = Stroke::NONE;
        w.inactive.fg_stroke = Stroke::new(1.0, INK_DIM);
        w.inactive.corner_radius = r;
        w.inactive.expansion = 0.0;

        w.hovered.bg_fill = CONTROL_HI;
        w.hovered.weak_bg_fill = HOVER;
        w.hovered.bg_stroke = Stroke::NONE;
        w.hovered.fg_stroke = Stroke::new(1.0, INK);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;

        w.active.bg_fill = INK_DIM;
        w.active.weak_bg_fill = CONTROL;
        w.active.bg_stroke = Stroke::NONE;
        w.active.fg_stroke = Stroke::new(1.0, INK);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;

        w.open.bg_fill = CONTROL;
        w.open.weak_bg_fill = HOVER;
        w.open.bg_stroke = Stroke::NONE;
        w.open.fg_stroke = Stroke::new(1.0, INK);
        w.open.corner_radius = r;
    });
}
