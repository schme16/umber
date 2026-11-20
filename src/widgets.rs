//! Custom-painted Graphite widgets. Every hit target is at least 30 px tall at 100 %
//! and grows with the UI scale, so pen use stays comfortable on small displays.

use egui::{Align2, Color32, CornerRadius, Response, Sense, Stroke, Ui, Vec2};

use crate::theme::{self, *};

/// Square icon button. `selected` fills it with the accent.
pub fn icon_button(ui: &mut Ui, icon: char, selected: bool, size: f32, tooltip: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let painter = ui.painter_at(rect.expand(1.0));
    let (bg, fg) = if selected {
        (ACCENT, ON_ACCENT)
    } else if response.hovered() {
        (RAISED, TEXT)
    } else {
        (Color32::TRANSPARENT, ICON)
    };
    painter.rect_filled(rect, CornerRadius::same(10), bg);
    painter.text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon_font(size * 0.46), fg);
    response.on_hover_text(tooltip)
}

/// Plain icon button that greys out when `enabled` is false (and then ignores clicks).
pub fn icon_action(ui: &mut Ui, icon: char, enabled: bool, size: f32, tooltip: &str) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), sense);
    let painter = ui.painter_at(rect.expand(1.0));
    let fg = if !enabled {
        ICON_OFF
    } else if response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(10), RAISED);
        TEXT
    } else {
        ICON
    };
    painter.text(rect.center(), Align2::CENTER_CENTER, icon, theme::icon_font(size * 0.46), fg);
    response.on_hover_text(tooltip)
}

/// Segmented control: one choice out of a few, shown side by side.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)], height: f32) -> bool {
    segmented_to(ui, value, options, height, None)
}

/// [`segmented`] stretched across the width left in `ui`, the spare room shared out evenly.
pub fn segmented_fill<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)], height: f32) -> bool {
    let width = ui.available_width();
    segmented_to(ui, value, options, height, Some(width))
}

fn segmented_to<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)], height: f32, fill: Option<f32>) -> bool {
    let font = theme::medium(13.0);
    let chrome = 6.0 + 2.0 * (options.len() as f32 - 1.0);
    let text: Vec<f32> = options
        .iter()
        .map(|(_, label)| ui.ctx().fonts_mut(|f| f.layout_no_wrap(label.to_string(), font.clone(), TEXT).size().x))
        .collect();
    let pad = match fill {
        // Never tighter than 6 a side, even if that overflows.
        Some(width) => ((width - chrome - text.iter().sum::<f32>()) / (2.0 * options.len() as f32)).max(6.0),
        None => 14.0,
    };
    let widths: Vec<f32> = text.iter().map(|t| t + pad * 2.0).collect();
    let total = widths.iter().sum::<f32>() + chrome;
    let (outer, _) = ui.allocate_exact_size(egui::vec2(total, height), Sense::hover());
    let painter = ui.painter_at(outer.expand(1.0));
    painter.rect(outer, CornerRadius::same(10), SEG_BG, Stroke::new(1.0, TOPBAR_LINE), egui::StrokeKind::Inside);

    let mut changed = false;
    let mut x = outer.left() + 3.0;
    for ((opt, label), w) in options.iter().zip(widths) {
        let r = egui::Rect::from_min_size(egui::pos2(x, outer.top() + 3.0), egui::vec2(w, height - 6.0));
        let resp = ui.interact(r, ui.scope_id().with(("seg", label)), Sense::click());
        let on = *value == *opt;
        if on {
            painter.rect_filled(r, CornerRadius::same(7), SEG_ON);
        } else if resp.hovered() {
            painter.rect_filled(r, CornerRadius::same(7), Color32::from_rgb(0x22, 0x1e, 0x19));
        }
        let color = if on { Color32::from_rgb(0xf6, 0xef, 0xe3) } else { Color32::from_rgb(0xa8, 0x9a, 0x87) };
        painter.text(r.center(), Align2::CENTER_CENTER, *label, font.clone(), color);
        if resp.clicked() && !on {
            *value = *opt;
            changed = true;
        }
        x += w + 2.0;
    }
    changed
}

/// Toggle chip: small pill that is either on (amber tint) or off (outlined).
pub fn chip(ui: &mut Ui, label: &str, on: &mut bool) -> Response {
    let font = theme::medium(12.0);
    let text_w = ui.ctx().fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font.clone(), TEXT).size().x);
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(text_w + 20.0, 28.0), Sense::click());
    let painter = ui.painter_at(rect.expand(1.0));
    let (bg, line, fg) = if *on {
        (ACCENT_SOFT_BG, ACCENT_SOFT_LINE, ACCENT_SOFT_TEXT)
    } else if response.hovered() {
        (RAISED, TRACK, TEXT)
    } else {
        (Color32::TRANSPARENT, TRACK, TEXT_2)
    };
    painter.rect(rect, CornerRadius::same(8), bg, Stroke::new(1.0, line), egui::StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, font, fg);
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response
}

/// Width [`button`] will take for `label`, for centring rows of buttons.
pub fn button_width(ctx: &egui::Context, label: &str) -> f32 {
    ctx.fonts_mut(|f| f.layout_no_wrap(label.to_owned(), theme::semibold(13.0), TEXT).size().x) + 32.0
}

/// Plain push button. `primary` uses the accent fill.
pub fn button(ui: &mut Ui, label: &str, primary: bool, height: f32) -> Response {
    let width = button_width(ui.ctx(), label);
    button_sized(ui, label, primary, width, height)
}

/// A push button as wide as the space it's in, for the one action a panel exists for.
pub fn button_full(ui: &mut Ui, label: &str, primary: bool, height: f32) -> Response {
    let width = ui.available_width();
    button_sized(ui, label, primary, width, height)
}

fn button_sized(ui: &mut Ui, label: &str, primary: bool, width: f32, height: f32) -> Response {
    let font = theme::semibold(13.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    let painter = ui.painter_at(rect.expand(1.0));
    let (bg, fg) = match (primary, response.hovered()) {
        (true, false) => (ACCENT, ON_ACCENT),
        (true, true) => (Color32::from_rgb(0xdd, 0xb6, 0x7c), ON_ACCENT),
        (false, false) => (RAISED, TEXT),
        (false, true) => (SEG_ON, TEXT),
    };
    painter.rect_filled(rect, CornerRadius::same(9), bg);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, font, fg);
    response
}

/// Small uppercase section label.
pub fn caps_label(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text.to_uppercase()).font(theme::medium(11.0)).color(TEXT_3).extra_letter_spacing(0.6));
}

/// Horizontal track slider with a round knob. Drag anywhere on the track.
pub fn track_slider(ui: &mut Ui, value: &mut f32, range: std::ops::RangeInclusive<f32>, width: f32) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(width, 24.0), Sense::click_and_drag());
    let (lo, hi) = (*range.start(), *range.end());
    if let Some(pos) = response.interact_pointer_pos() {
        if response.dragged() || response.clicked() {
            let t = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            let new = lo + t * (hi - lo);
            if new != *value {
                *value = new;
                response.mark_changed();
            }
        }
    }
    let t = ((*value - lo) / (hi - lo)).clamp(0.0, 1.0);
    let painter = ui.painter_at(rect.expand(8.0));
    let track = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 4.0));
    painter.rect_filled(track, CornerRadius::same(2), TRACK);
    let mut filled = track;
    filled.set_right(track.left() + track.width() * t);
    painter.rect_filled(filled, CornerRadius::same(2), ACCENT);
    let knob = egui::pos2(track.left() + track.width() * t, track.center().y);
    let r = if response.hovered() || response.dragged() { 9.0 } else { 8.0 };
    painter.circle_filled(knob + egui::vec2(0.0, 1.0), r, Color32::from_black_alpha(90));
    painter.circle_filled(knob, r, TEXT);
    response
}

/// Thin vertical divider for toolbars.
pub fn vdivider(ui: &mut Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, height), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, PANEL_LINE);
}

/// Width `segmented` will take for these labels, so callers can centre it.
pub fn segmented_width(ctx: &egui::Context, labels: &[&str]) -> f32 {
    let font = theme::medium(13.0);
    let text: f32 =
        labels.iter().map(|l| ctx.fonts_mut(|f| f.layout_no_wrap(l.to_string(), font.clone(), TEXT).size().x)).sum();
    text + 28.0 * labels.len() as f32 + 6.0 + 2.0 * (labels.len() as f32 - 1.0)
}

/// Pressure curve graph: pen pressure across, brush strength up. `live` puts a dot on the
/// curve at the pen's current pressure. Clickable, so it can open the curve settings.
pub fn pressure_curve(ui: &mut Ui, size: egui::Vec2, curve: &crate::pen::PressureCurve, live: Option<f32>) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter_at(rect.expand(2.0));
    let bg = if response.hovered() { SEG_ON } else { RAISED };
    painter.rect(rect, CornerRadius::same(6), bg, Stroke::new(1.0, TRACK), egui::StrokeKind::Inside);
    let inner = rect.shrink((size.y * 0.16).max(4.0));
    let at = |p: f32| egui::pos2(inner.left() + inner.width() * p, inner.bottom() - inner.height() * curve.apply(p));
    painter.line_segment([inner.left_bottom(), inner.right_top()], Stroke::new(1.0, TRACK));
    let points: Vec<_> = (0..=32).map(|i| at(i as f32 / 32.0)).collect();
    painter.add(egui::Shape::line(points, Stroke::new(if size.y > 40.0 { 2.0 } else { 1.5 }, ACCENT)));
    if let Some(p) = live {
        painter.circle_filled(at(p), if size.y > 40.0 { 5.0 } else { 3.0 }, TEXT);
    }
    response
}

/// Pressed or clicked: for choosing a tool or option, which is safe to do on the press. A pen
/// tap often slides more than egui allows for a click, and this still lands.
pub fn tapped(response: &Response) -> bool {
    response.clicked() || (response.is_pointer_button_down_on() && response.ctx.input(|i| i.pointer.primary_pressed()))
}
