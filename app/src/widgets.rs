//! Small chromeless widgets in the Darkroom Lightbox language.

use crate::theme::{self, *};
use egui::{Align, Color32, CornerRadius, Layout, Response, RichText, Sense, Ui, Vec2};

/// Label on the left, control right-aligned. Fixed row height keeps the column rhythm.
pub fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    row_tip(ui, label, "", add);
}

/// [`row`] with an explanation on the label (dotted underline hints it's hoverable).
pub fn row_tip(ui: &mut Ui, label: &str, tip: &str, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 28.0), Layout::left_to_right(Align::Center), |ui| {
        let r = ui.label(RichText::new(label).color(SECONDARY));
        if !tip.is_empty() {
            let y = r.rect.bottom() - 1.0;
            let mut x = r.rect.left();
            while x < r.rect.right() {
                ui.painter().line_segment([egui::pos2(x, y), egui::pos2((x + 1.5).min(r.rect.right()), y)], egui::Stroke::new(1.0, CONTROL_HI));
                x += 3.5;
            }
            r.on_hover_text(tip);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), add);
    });
}

/// Like [`row`], but the control is laid out left-to-right in a fixed-width slot at the
/// right edge (sliders keep their value box after the track).
pub fn row_ltr(ui: &mut Ui, label: &str, width: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 28.0), Layout::left_to_right(Align::Center), |ui| {
        ui.label(RichText::new(label).color(SECONDARY));
        let rest = ui.available_width();
        ui.add_space((rest - width).max(0.0));
        ui.allocate_ui_with_layout(Vec2::new(width.min(rest), 28.0), Layout::left_to_right(Align::Center), add);
    });
}

pub fn value(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.label(RichText::new(text.into()).font(theme::mono(12.0)).color(INK))
}

/// Segmented control. Returns true when the value changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let font = theme::ui(12.0);
    let pad = Vec2::new(9.0, 4.0);
    let widths: Vec<f32> = options
        .iter()
        .map(|(_, l)| ui.fonts_mut(|f| f.layout_no_wrap(l.to_string(), font.clone(), INK).size().x) + pad.x * 2.0)
        .collect();
    let total = widths.iter().sum::<f32>() + 4.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total, 24.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(RADIUS), WELL);
    let mut x = rect.left() + 2.0;
    let mut changed = false;
    for ((v, label), w) in options.iter().zip(widths) {
        let r = egui::Rect::from_min_size(egui::pos2(x, rect.top() + 2.0), Vec2::new(w, rect.height() - 4.0));
        let resp = ui.interact(r, ui.scope_id().with(("seg", label)), Sense::click());
        let on = *value == *v;
        if on {
            ui.painter().rect_filled(r, CornerRadius::same(4), CONTROL);
        } else if resp.hovered() {
            ui.painter().rect_filled(r, CornerRadius::same(4), HOVER);
        }
        let color = if on || resp.hovered() { INK } else { SECONDARY };
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, *label, font.clone(), color);
        if resp.clicked() && !on {
            *value = *v;
            changed = true;
        }
        x += w;
    }
    changed
}

/// Round icon button with press feedback (slight scale-down while held).
pub fn icon_button(ui: &mut Ui, icon: &str, tooltip: &str, size: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let held = resp.is_pointer_button_down_on();
    if resp.hovered() || held {
        ui.painter().rect_filled(rect, CornerRadius::same(RADIUS), if held { CONTROL } else { HOVER });
    }
    let scale = if held { 0.94 } else { 1.0 };
    let color = if resp.hovered() { INK } else { INK_DIM };
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, theme::ui(size * 0.52 * scale), color);
    resp.on_hover_text(tooltip)
}

/// Filled primary action (ink on room).
pub fn primary_button(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    let font = theme::strong(13.0);
    let galley = ui.fonts_mut(|f| f.layout_no_wrap(text.to_string(), font.clone(), INK));
    let size = galley.size() + Vec2::new(28.0, 14.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let held = resp.is_pointer_button_down_on() && enabled;
    let r = if held { rect.shrink(1.0) } else { rect };
    let fill = if !enabled {
        CONTROL
    } else if resp.hovered() {
        Color32::WHITE
    } else {
        INK
    };
    ui.painter().rect_filled(r, CornerRadius::same(RADIUS), fill);
    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, text, font, if enabled { ROOM } else { MUTED });
    resp
}

/// Quiet secondary action (well fill).
pub fn quiet_button(ui: &mut Ui, text: &str) -> Response {
    let font = theme::ui(12.5);
    let galley = ui.fonts_mut(|f| f.layout_no_wrap(text.to_string(), font.clone(), INK));
    let size = galley.size() + Vec2::new(20.0, 12.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let held = resp.is_pointer_button_down_on();
    let r = if held { rect.shrink(1.0) } else { rect };
    ui.painter().rect_filled(r, CornerRadius::same(RADIUS), if resp.hovered() { CONTROL } else { HOVER });
    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, text, font, if resp.hovered() { INK } else { INK_DIM });
    resp
}

/// Small keycap hint, e.g. "G".
pub fn keycap(ui: &Ui, painter: &egui::Painter, pos: egui::Pos2, key: &str) -> egui::Rect {
    let font = theme::mono(10.5);
    let g = ui.fonts_mut(|f| f.layout_no_wrap(key.to_string(), font.clone(), SECONDARY));
    let r = egui::Rect::from_min_size(pos, g.size() + Vec2::new(8.0, 4.0));
    painter.rect_stroke(r, CornerRadius::same(3), egui::Stroke::new(1.0, CONTROL_HI), egui::StrokeKind::Inside);
    painter.text(r.center(), egui::Align2::CENTER_CENTER, key, font, SECONDARY);
    r
}

/// Darkroom slider: 2px rail, quiet trailing fill, small solid knob that grows while
/// hovered or dragged, value text after the track.
/// - Double-click the track resets to `default`.
/// - Hold Shift or Alt while dragging for fine adjustment (1/10 speed, finer steps).
/// - Click the value to type one (Enter applies, Esc cancels).
pub struct Slider<'a> {
    pub value: &'a mut f32,
    pub range: std::ops::RangeInclusive<f32>,
    pub default: f32,
    pub step: f32,
    pub log: bool,
    pub text: &'a dyn Fn(f32) -> String,
}

impl Slider<'_> {
    pub fn show(self, ui: &mut Ui, width: f32) -> Response {
        let text_w = 52.0;
        let (rect, alloc) = ui.allocate_exact_size(Vec2::new(width, 20.0), Sense::hover());
        let id = alloc.id;
        let track_rect = egui::Rect::from_min_max(rect.min, egui::pos2(rect.right() - text_w - 2.0, rect.max.y));
        let text_rect = egui::Rect::from_min_max(egui::pos2(rect.right() - text_w, rect.min.y), rect.max);
        let mut resp = ui.interact(track_rect, id.with("track"), Sense::click_and_drag());
        let track = egui::Rect::from_min_max(egui::pos2(rect.left() + 6.0, rect.center().y), egui::pos2(rect.right() - text_w - 8.0, rect.center().y));
        let (lo, hi) = (*self.range.start(), *self.range.end());
        let to_t = |v: f32| -> f32 {
            if self.log {
                ((v.max(1e-6) / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
            } else {
                ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
            }
        };
        let quantize = |v: f32, step: f32| if step > 0.0 { (v / step).round() * step } else { v };
        let fine = ui.input(|i| i.modifiers.shift || i.modifiers.alt);
        let step = if fine { self.step / 5.0 } else { self.step };
        let from_t = |t: f32| -> f32 { quantize(if self.log { lo * (hi / lo).powf(t) } else { lo + t * (hi - lo) }, step) };

        // The unquantised track position is kept while dragging so slow fine drags accumulate.
        let drag_key = id.with("t");
        if resp.double_clicked() {
            *self.value = self.default;
            resp.mark_changed();
        } else if resp.dragged() || resp.clicked() {
            let mut t: f32 = ui.data(|d| d.get_temp(drag_key)).filter(|_| !resp.drag_started()).unwrap_or_else(|| to_t(*self.value));
            if fine && resp.dragged() {
                t += resp.drag_delta().x / track.width() * 0.1;
            } else if let Some(p) = resp.interact_pointer_pos() {
                t = (p.x - track.left()) / track.width();
            }
            let t = t.clamp(0.0, 1.0);
            ui.data_mut(|d| d.insert_temp(drag_key, t));
            let nv = from_t(t).clamp(lo, hi);
            if nv != *self.value {
                *self.value = nv;
                resp.mark_changed();
            }
        }
        if resp.drag_stopped() {
            ui.data_mut(|d| d.remove::<f32>(drag_key));
        }

        // Typed entry.
        let edit_key = id.with("edit");
        let mut editing: Option<String> = ui.data(|d| d.get_temp(edit_key));
        let text_resp = ui.interact(text_rect, id.with("value"), Sense::click());
        if editing.is_none() && text_resp.clicked() {
            // Start from the plain number shown ("5600K" -> "5600", "+0.50" -> "0.50").
            let shown = (self.text)(*self.value);
            let plain: String = shown.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
            editing = Some(plain);
        }
        let t = to_t(*self.value);
        let x = track.left() + t * track.width();
        let p = ui.painter();
        p.line_segment([track.left_center(), track.right_center()], egui::Stroke::new(2.0, CONTROL));
        p.line_segment([track.left_center(), egui::pos2(x, track.center().y)], egui::Stroke::new(2.0, SECONDARY));
        let active = resp.hovered() || resp.dragged();
        p.circle_filled(egui::pos2(x, track.center().y), if active { 7.0 } else { 6.0 }, if active { INK } else { INK_DIM });
        if let Some(mut buf) = editing {
            let te = egui::TextEdit::singleline(&mut buf)
                .font(theme::mono(12.0))
                .align(egui::Align2::RIGHT_CENTER)
                .margin(egui::Margin::symmetric(4, 1))
                .desired_width(text_w - 8.0);
            let r = ui.put(text_rect, te);
            if !r.has_focus() && !r.lost_focus() {
                r.request_focus();
            }
            let (enter, escape) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
            if escape {
                ui.data_mut(|d| d.remove::<String>(edit_key));
            } else if enter || r.lost_focus() {
                if let Ok(v) = buf.trim().trim_start_matches('+').parse::<f32>() {
                    let nv = quantize(v, self.step / 5.0).clamp(lo, hi);
                    if nv != *self.value {
                        *self.value = nv;
                        resp.mark_changed();
                    }
                }
                ui.data_mut(|d| d.remove::<String>(edit_key));
            } else {
                ui.data_mut(|d| d.insert_temp(edit_key, buf));
            }
        } else {
            let hovered = text_resp.hovered();
            if hovered {
                // The fill grows around the number; the number itself never moves.
                let fill = egui::Rect::from_min_max(egui::pos2(text_rect.left() - 2.0, text_rect.top() + 1.0), egui::pos2(text_rect.right() + 5.0, text_rect.bottom() - 1.0));
                ui.painter().rect_filled(fill, CornerRadius::same(4), HOVER);
                ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
            }
            ui.painter().text(egui::pos2(rect.right(), rect.center().y), egui::Align2::RIGHT_CENTER, (self.text)(*self.value), theme::mono(12.0), INK);
            text_resp.on_hover_text("Click to type a value");
        }
        resp.on_hover_text("Drag to adjust · Shift or Alt for fine · double-click to reset")
    }
}

/// Phosphor caret for every ComboBox (one icon family across the app).
pub fn combo_caret(ui: &Ui, rect: egui::Rect, _v: &egui::style::WidgetVisuals, open: bool) {
    let icon = if open { "\u{E13C}" } else { theme::icon::CARET_DOWN };
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, theme::ui(12.0), SECONDARY);
}

/// Shorten `text` with a trailing ellipsis so it fits in `max_w` points.
pub fn elide(ui: &Ui, text: &str, font: &egui::FontId, max_w: f32) -> String {
    let width = |s: &str| ui.fonts_mut(|f| f.layout_no_wrap(s.to_string(), font.clone(), INK).size().x);
    if width(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&candidate) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
}
