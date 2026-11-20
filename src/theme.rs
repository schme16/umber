//! Direction A layout (dark ground, full-bleed viewport, floating rounded panels) in the
//! "Umber and brass" palette: warm charcoal, ivory text, one burnished brass accent. The
//! viewport itself stays neutral grey so it never skews colour judgement.

use std::sync::Arc;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle};

pub const GROUND: Color32 = Color32::from_rgb(0x11, 0x0f, 0x0d);
pub const TOPBAR: Color32 = Color32::from_rgb(0x15, 0x12, 0x0f);
pub const TOPBAR_LINE: Color32 = Color32::from_rgb(0x2a, 0x25, 0x1f);
pub const VIEWPORT: Color32 = Color32::from_rgb(0x1b, 0x1b, 0x1c);
pub const PANEL: Color32 = Color32::from_rgb(0x1d, 0x19, 0x15);
pub const PANEL_LINE: Color32 = Color32::from_rgb(0x33, 0x2c, 0x25);
pub const RAISED: Color32 = Color32::from_rgb(0x2a, 0x24, 0x1e);
pub const SEG_BG: Color32 = Color32::from_rgb(0x17, 0x14, 0x0f);
pub const SEG_ON: Color32 = Color32::from_rgb(0x35, 0x2d, 0x25);
pub const TRACK: Color32 = Color32::from_rgb(0x3a, 0x33, 0x2b);

pub const TEXT: Color32 = Color32::from_rgb(0xf1, 0xe9, 0xdc);
pub const TEXT_2: Color32 = Color32::from_rgb(0xcd, 0xbf, 0xac);
pub const TEXT_3: Color32 = Color32::from_rgb(0xa3, 0x95, 0x82);
pub const ICON: Color32 = Color32::from_rgb(0xbc, 0xae, 0x9a);
pub const ICON_OFF: Color32 = Color32::from_rgb(0x5a, 0x52, 0x49);

pub const ACCENT: Color32 = Color32::from_rgb(0xc9, 0xa0, 0x63);
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x1b, 0x13, 0x09);
pub const ACCENT_SOFT_BG: Color32 = Color32::from_rgb(0x2e, 0x26, 0x1b);
pub const ACCENT_SOFT_LINE: Color32 = Color32::from_rgb(0x7d, 0x64, 0x40);
pub const ACCENT_SOFT_TEXT: Color32 = Color32::from_rgb(0xe6, 0xc8, 0x96);

pub const ICONS: &str = "icons";
pub const MONO: &str = "mono";
pub const MEDIUM: &str = "medium";
pub const SEMIBOLD: &str = "semibold";

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Body, FontId::proportional(13.0)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Heading, FontId::new(15.0, FontFamily::Name(SEMIBOLD.into()))),
            (TextStyle::Monospace, FontId::new(12.0, FontFamily::Name(MONO.into()))),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.spacing.interact_size.y = 30.0;

        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = PANEL;
        v.window_fill = PANEL;
        v.extreme_bg_color = Color32::from_rgb(0x14, 0x11, 0x0e);
        v.faint_bg_color = RAISED;
        v.override_text_color = Some(TEXT);
        v.window_stroke = Stroke::new(1.0, PANEL_LINE);
        v.window_corner_radius = CornerRadius::same(14);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_shadow = panel_shadow();
        v.popup_shadow = panel_shadow();
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, ON_ACCENT);
        v.hyperlink_color = ACCENT;
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(8);
        }
        v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        v.widgets.inactive.bg_fill = RAISED;
        v.widgets.hovered.weak_bg_fill = RAISED;
        v.widgets.hovered.bg_fill = SEG_ON;
        v.widgets.active.weak_bg_fill = SEG_ON;
    });
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    add(&mut fonts, "inter", include_bytes!("../assets/fonts/Inter-Regular.ttf"));
    add(&mut fonts, "inter-medium", include_bytes!("../assets/fonts/Inter-Medium.ttf"));
    add(&mut fonts, "inter-semibold", include_bytes!("../assets/fonts/Inter-SemiBold.ttf"));
    add(&mut fonts, "jbmono", include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"));
    add(&mut fonts, "lucide", include_bytes!("../assets/fonts/lucide.ttf"));

    // Keep egui's bundled fonts as fallbacks for symbols Inter lacks.
    let fallback_prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let fallback_mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();

    let with_fallback = |first: &str, rest: &[String]| {
        let mut v = vec![first.to_owned()];
        v.extend(rest.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Proportional, with_fallback("inter", &fallback_prop));
    fonts.families.insert(FontFamily::Monospace, with_fallback("jbmono", &fallback_mono));
    fonts.families.insert(FontFamily::Name(MEDIUM.into()), with_fallback("inter-medium", &fallback_prop));
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), with_fallback("inter-semibold", &fallback_prop));
    fonts.families.insert(FontFamily::Name(MONO.into()), with_fallback("jbmono", &fallback_mono));
    fonts.families.insert(FontFamily::Name(ICONS.into()), vec!["lucide".to_owned()]);
    ctx.set_fonts(fonts);
}

pub fn panel_shadow() -> Shadow {
    Shadow { offset: [0, 12], blur: 32, spread: 0, color: Color32::from_black_alpha(115) }
}

/// The rounded, floating panel used for every tool surface over the viewport.
pub fn panel_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, PANEL_LINE))
        .corner_radius(CornerRadius::same(14))
        .shadow(panel_shadow())
        .inner_margin(Margin::same(8))
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MEDIUM.into()))
}
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MONO.into()))
}
