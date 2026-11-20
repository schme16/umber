//! Finding things: the command palette, the shortcut sheet, and tips that show once.
//! All of it reads from [`crate::commands`], so nothing here can name a key that doesn't work.

use std::path::PathBuf;

use egui::{Align, Align2, Color32, CornerRadius, Layout, Margin, Sense, Stroke, Vec2};

use crate::commands::{self, Cmd, Group, COMMANDS};
use crate::theme::{self, *};
use crate::widgets;

/// What the palette needs to know about a command right now.
pub struct CmdState {
    /// Why it can't run, if it can't.
    pub reason: Option<String>,
    /// For commands that pick a tool, view or setting: whether it's the current one.
    pub on: Option<bool>,
}

// ---------------------------------------------------------------- palette

#[derive(Default)]
pub struct Palette {
    pub open: bool,
    query: String,
    selected: usize,
    /// Most recent first; this session only.
    recent: Vec<Cmd>,
    /// Scroll the selected row into view on the next frame.
    reveal: bool,
}

impl Palette {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.query.clear();
        self.selected = 0;
    }

    fn rows(&self) -> Vec<(Option<&'static str>, Cmd)> {
        if !self.query.trim().is_empty() {
            return commands::search(&self.query).into_iter().map(|c| (None, c)).collect();
        }
        let mut rows: Vec<(Option<&'static str>, Cmd)> = Vec::new();
        for (i, c) in self.recent.iter().enumerate() {
            rows.push(((i == 0).then_some("Recent"), *c));
        }
        for group in Group::ALL {
            let mut first = true;
            for c in COMMANDS.iter().filter(|c| c.group == group) {
                rows.push((first.then_some(group.name()), c.cmd));
                first = false;
            }
        }
        rows
    }

    /// Draw it if open. Returns a command to run.
    pub fn show(&mut self, ctx: &egui::Context, state: &dyn Fn(Cmd) -> CmdState) -> Option<Cmd> {
        if !self.open {
            return None;
        }
        let rows = self.rows();
        let (up, down, enter) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            )
        });
        if !rows.is_empty() {
            if up {
                self.selected = (self.selected + rows.len() - 1) % rows.len();
                self.reveal = true;
            }
            if down {
                self.selected = (self.selected + 1) % rows.len();
                self.reveal = true;
            }
        }
        self.selected = self.selected.min(rows.len().saturating_sub(1));

        let mut chosen = None;
        let id = egui::Id::unique("command palette");
        let area = egui::Modal::default_area(id).anchor(Align2::CENTER_TOP, egui::vec2(0.0, 84.0));
        let modal = egui::Modal::new(id)
            .area(area)
            .backdrop_color(Color32::from_black_alpha(90))
            .frame(theme::panel_frame().inner_margin(Margin::same(10)))
            .show(ctx, |ui| {
                ui.set_width(560.0);
                let edit = egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Type what you want to do…")
                    .font(theme::medium(15.0))
                    .frame(egui::Frame::new().fill(SEG_BG).corner_radius(CornerRadius::same(8)).inner_margin(Margin::symmetric(12, 10)))
                    .desired_width(f32::INFINITY);
                let resp = ui.add(edit);
                resp.request_focus();
                if resp.changed() {
                    self.selected = 0;
                    self.reveal = true;
                }
                ui.add_space(6.0);
                if rows.is_empty() {
                    ui.add_space(10.0);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("Nothing by that name yet.").color(TEXT_2));
                        let sheet = commands::key_label(Cmd::Shortcuts).unwrap_or_default();
                        ui.label(egui::RichText::new(format!("Try another word, or {sheet} for every key.")).color(TEXT_3).size(12.0));
                    });
                    ui.add_space(12.0);
                    return;
                }
                egui::ScrollArea::vertical().max_height(440.0).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (i, (header, cmd)) in rows.iter().enumerate() {
                        if let Some(h) = header {
                            ui.add_space(if i == 0 { 2.0 } else { 8.0 });
                            ui.horizontal(|ui| {
                                ui.add_space(10.0);
                                widgets::caps_label(ui, h);
                            });
                            ui.add_space(2.0);
                        }
                        let st = state(*cmd);
                        let resp = palette_row(ui, *cmd, &st, i == self.selected);
                        if i == self.selected && self.reveal {
                            resp.scroll_to_me(None);
                        }
                        if resp.hovered() && ui.input(|i| i.pointer.delta() != Vec2::ZERO) {
                            self.selected = i;
                        }
                        if resp.clicked() && st.reason.is_none() {
                            chosen = Some(*cmd);
                        }
                    }
                });
                self.reveal = false;
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    let note = "↑↓ to choose · Enter to run · Esc to close";
                    ui.label(egui::RichText::new(note).color(TEXT_3).size(11.5));
                });
            });
        if enter {
            if let Some((_, cmd)) = rows.get(self.selected) {
                if state(*cmd).reason.is_none() {
                    chosen = Some(*cmd);
                }
            }
        }
        if modal.should_close() || chosen.is_some() {
            self.open = false;
            self.query.clear();
        }
        if let Some(cmd) = chosen {
            self.recent.retain(|c| *c != cmd);
            self.recent.insert(0, cmd);
            self.recent.truncate(4);
        }
        chosen
    }
}

/// One command: its name and what it does (or why it can't), with its keys on the right.
fn palette_row(ui: &mut egui::Ui, cmd: Cmd, st: &CmdState, selected: bool) -> egui::Response {
    let c = commands::get(cmd);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(8), RAISED);
    }
    let off = st.reason.is_some();
    let x = rect.left() + 12.0;
    let mut name_x = x;
    if let Some(on) = st.on {
        let dot = egui::pos2(x + 4.0, rect.top() + 15.0);
        if on {
            p.circle_filled(dot, 4.0, ACCENT);
        } else {
            p.circle_stroke(dot, 3.5, Stroke::new(1.0, TEXT_3));
        }
        name_x += 16.0;
    }
    p.text(egui::pos2(name_x, rect.top() + 15.0), Align2::LEFT_CENTER, c.name, theme::medium(14.0), if off { TEXT_3 } else { TEXT });
    let (detail, colour) = match &st.reason {
        Some(r) => (r.as_str(), ACCENT_SOFT_TEXT),
        None => (c.about, TEXT_3),
    };
    let keys_w = c.keys.first().map_or(0.0, |k| keycaps_width(ui, *k)) + 12.0;
    let room = rect.width() - (name_x - rect.left()) - keys_w - 8.0;
    // One line only; a long sentence ends in an ellipsis.
    let mut job = egui::text::LayoutJob::simple_singleline(detail.to_owned(), egui::FontId::proportional(12.0), colour);
    job.wrap = egui::text::TextWrapping::truncate_at_width(room.max(80.0));
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(egui::pos2(name_x, rect.top() + 25.0), galley, colour);
    if let Some(k) = c.keys.first() {
        let w = keycaps_width(ui, *k);
        keycaps(ui, egui::pos2(rect.right() - 12.0 - w, rect.center().y), *k, !off);
    }
    resp
}

const CAP_FONT: f32 = 11.5;

fn cap_width(ui: &egui::Ui, text: &str) -> f32 {
    let w = ui.painter().layout_no_wrap(text.to_owned(), theme::mono(CAP_FONT), TEXT).size().x;
    (w + 12.0).max(22.0)
}

fn keycaps_width(ui: &egui::Ui, k: commands::Shortcut) -> f32 {
    let caps = k.caps();
    caps.iter().map(|c| cap_width(ui, c)).sum::<f32>() + 4.0 * (caps.len() as f32 - 1.0)
}

/// Draw a shortcut as keycaps, left edge at `at.x`, centred on `at.y`.
fn keycaps(ui: &egui::Ui, at: egui::Pos2, k: commands::Shortcut, bright: bool) {
    let p = ui.painter();
    let mut x = at.x;
    for cap in k.caps() {
        let w = cap_width(ui, &cap);
        let r = egui::Rect::from_min_size(egui::pos2(x, at.y - 11.0), egui::vec2(w, 22.0));
        p.rect_filled(r, CornerRadius::same(5), SEG_BG);
        p.rect_stroke(r, CornerRadius::same(5), Stroke::new(1.0, PANEL_LINE), egui::StrokeKind::Inside);
        p.text(r.center(), Align2::CENTER_CENTER, cap, theme::mono(CAP_FONT), if bright { TEXT_2 } else { TEXT_3 });
        x += w + 4.0;
    }
}

// ---------------------------------------------------------------- shortcut sheet

/// Mouse and pen moves, which aren't commands but belong on the sheet.
const GESTURES: &[(&str, &str)] = &[
    ("Turn the model", "Right-drag, or hold Space or Alt and drag; it turns about the point you grab"),
    ("Move the model", "Shift+right-drag"),
    ("Zoom", "Scroll"),
    ("Fill the whole material", "Shift+click with Fill"),
];

/// Every command that has a key, by group. Returns a command if one was clicked.
pub fn shortcut_sheet(ctx: &egui::Context, open: &mut bool) -> Option<Cmd> {
    if !*open {
        return None;
    }
    let mut chosen = None;
    let id = egui::Id::unique("shortcut sheet");
    let modal = egui::Modal::new(id)
        .backdrop_color(Color32::from_black_alpha(90))
        .frame(theme::panel_frame().inner_margin(Margin::same(24)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Keyboard shortcuts").font(theme::semibold(17.0)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, crate::icons::X, false, 30.0, "Close (Esc)").clicked() {
                        *open = false;
                    }
                });
            });
            ui.add_space(14.0);
            let col_w = 290.0;
            let groups: Vec<Group> = Group::ALL.into_iter().filter(|g| COMMANDS.iter().any(|c| c.group == *g && !c.keys.is_empty())).collect();
            // Three columns, groups flowing down each in turn.
            let columns: [&[Group]; 3] = {
                let n = groups.len();
                let a = n.div_ceil(3);
                let b = (n - a).div_ceil(2);
                [&groups[..a], &groups[a..a + b], &groups[a + b..]]
            };
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 28.0;
                for (ci, col) in columns.iter().enumerate() {
                    ui.vertical(|ui| {
                        ui.set_width(col_w);
                        for g in col.iter() {
                            widgets::caps_label(ui, g.name());
                            ui.add_space(4.0);
                            for c in COMMANDS.iter().filter(|c| c.group == *g && !c.keys.is_empty()) {
                                let (row, resp) = ui.allocate_exact_size(egui::vec2(col_w, 28.0), Sense::click());
                                if resp.hovered() {
                                    ui.painter().rect_filled(row, CornerRadius::same(6), RAISED);
                                }
                                ui.painter().text(egui::pos2(row.left() + 6.0, row.center().y), Align2::LEFT_CENTER, c.name, egui::FontId::proportional(13.0), TEXT_2);
                                let k = c.keys[0];
                                keycaps(ui, egui::pos2(row.right() - 6.0 - keycaps_width(ui, k), row.center().y), k, true);
                                let mut tip = c.about.to_owned();
                                if c.keys.len() > 1 {
                                    let more: Vec<String> = c.keys[1..].iter().map(|k| k.label()).collect();
                                    tip.push_str(&format!("\nAlso {}", more.join(", ")));
                                }
                                if resp.on_hover_text(tip).clicked() {
                                    chosen = Some(c.cmd);
                                }
                            }
                            ui.add_space(12.0);
                        }
                        if ci == 2 {
                            widgets::caps_label(ui, "Mouse and pen");
                            ui.add_space(4.0);
                            for (what, how) in GESTURES {
                                ui.horizontal(|ui| {
                                    ui.add_space(6.0);
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 1.0;
                                        ui.label(egui::RichText::new(*what).color(TEXT_2));
                                        ui.label(egui::RichText::new(*how).color(TEXT_3).size(12.0));
                                    });
                                });
                                ui.add_space(6.0);
                            }
                        }
                    });
                }
            });
            ui.add_space(6.0);
            let palette = commands::key_label(Cmd::Palette).unwrap_or_default();
            ui.label(egui::RichText::new(format!("Commands without a key are in the command palette: {palette}, then type what you want.")).color(TEXT_3).size(12.0));
        });
    if modal.should_close() || chosen.is_some() {
        *open = false;
    }
    chosen
}

// ---------------------------------------------------------------- tips

/// Something worth saying the first time a person meets it, and never again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tip {
    /// A model is open in the Paint workspace for the first time.
    Start,
    /// The eraser, picked up with a fill layer selected.
    EraseFill,
    Mask,
    Uv,
    Export,
}

impl Tip {
    #[cfg(test)]
    pub const ALL: [Tip; 5] = [Tip::Start, Tip::EraseFill, Tip::Mask, Tip::Uv, Tip::Export];

    fn id(self) -> &'static str {
        match self {
            Tip::Start => "start",
            Tip::EraseFill => "erase-fill",
            Tip::Mask => "mask",
            Tip::Uv => "uv",
            Tip::Export => "export",
        }
    }

    pub fn text(self) -> String {
        let key = |c| commands::key_label(c).unwrap_or_default();
        match self {
            Tip::Start => format!(
                "Paint with the pen or the left button. Right-drag turns the model, Shift+right-drag moves it, scrolling zooms; with only a pen, hold Space or Alt and drag. Press {} to find any command, or {} for every key.",
                key(Cmd::Palette),
                key(Cmd::Shortcuts)
            ),
            Tip::EraseFill => "A fill layer covers everything, so the eraser paints its mask instead: where you erase, the fill stops showing. The Mask tool paints it back.".to_owned(),
            Tip::Mask => format!(
                "Dark paint hides this layer and light paint shows it again. {} swaps your two colours, so you can switch between hiding and showing.",
                key(Cmd::SwapColours)
            ),
            Tip::Uv => "The square is the model's texture, unrolled: brass is where paint can go, red is where two parts share the same spot. Unwrap lays it out again, and your paint moves with it.".to_owned(),
            Tip::Export => format!(
                "Pick a folder inside your Godot project and the material finds its maps by res:// path. After the first export, {} exports again in one press.",
                key(Cmd::Export)
            ),
        }
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Prefs {
    tips_seen: Vec<String>,
    tips_off: bool,
    /// Image files the brush has painted with, newest first.
    recent_textures: Vec<PathBuf>,
    /// The F3 stats overlay was open when Umber last closed.
    stats_open: bool,
    /// How the camera turns, pans and zooms.
    camera: crate::camera::CameraPrefs,
    /// The folder of the last model or project opened or saved, where the next Open or Save
    /// as starts.
    last_folder: Option<PathBuf>,
}

/// Remembers which tips have been seen, in `$XDG_CONFIG_HOME/umber/prefs.json`.
pub struct Tips {
    prefs: Prefs,
    path: Option<PathBuf>,
}

fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("UMBER_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("umber"))
}

impl Tips {
    pub fn load() -> Self {
        let path = config_dir().map(|d| d.join("prefs.json"));
        let prefs = path.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { prefs, path }
    }

    fn write(&self) {
        let Some(path) = &self.path else { return };
        let write = || -> anyhow::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&self.prefs)?)?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        };
        if let Err(e) = write() {
            log::warn!("Couldn't save preferences to {}: {e:#}", path.display());
        }
    }

    pub fn wants(&self, tip: Tip) -> bool {
        !self.prefs.tips_off && !self.prefs.tips_seen.iter().any(|s| s == tip.id())
    }

    pub fn seen(&mut self, tip: Tip) {
        if self.wants(tip) {
            self.prefs.tips_seen.push(tip.id().to_owned());
            self.write();
        }
    }

    /// Some tip has been seen or switched off, so Show tips again has something to do.
    pub fn any_hidden(&self) -> bool {
        self.prefs.tips_off || !self.prefs.tips_seen.is_empty()
    }

    pub fn all_off(&mut self) {
        self.prefs.tips_off = true;
        self.write();
    }

    pub fn reset(&mut self) {
        self.prefs = Prefs {
            recent_textures: std::mem::take(&mut self.prefs.recent_textures),
            stats_open: self.prefs.stats_open,
            camera: self.prefs.camera,
            last_folder: self.prefs.last_folder.take(),
            ..Prefs::default()
        };
        self.write();
    }

    pub fn stats_open(&self) -> bool {
        self.prefs.stats_open
    }

    pub fn set_stats_open(&mut self, open: bool) {
        self.prefs.stats_open = open;
        self.write();
    }

    pub fn camera(&self) -> crate::camera::CameraPrefs {
        self.prefs.camera
    }

    pub fn set_camera(&mut self, camera: crate::camera::CameraPrefs) {
        if self.prefs.camera != camera {
            self.prefs.camera = camera;
            self.write();
        }
    }

    pub fn last_folder(&self) -> Option<&std::path::Path> {
        self.prefs.last_folder.as_deref().filter(|d| d.is_dir())
    }

    /// Open and Save as start in the folder `file` is in, from now on.
    pub fn remember_folder(&mut self, file: &std::path::Path) {
        let dir = file.parent().filter(|d| !d.as_os_str().is_empty()).map(std::path::Path::to_owned);
        if dir.is_some() && dir != self.prefs.last_folder {
            self.prefs.last_folder = dir;
            self.write();
        }
    }

    pub fn recent_textures(&self) -> &[PathBuf] {
        &self.prefs.recent_textures
    }

    /// Put `path` at the front of the recent brush textures (or take it off, when it's gone).
    pub fn remember_texture(&mut self, path: &std::path::Path, keep: bool) {
        self.prefs.recent_textures.retain(|p| p != path);
        if keep {
            self.prefs.recent_textures.insert(0, path.to_owned());
            self.prefs.recent_textures.truncate(8);
        }
        self.write();
    }
}

/// The tip card, centred at `pos`. Returns true if the tip was dismissed.
pub fn tip_card(ctx: &egui::Context, pos: egui::Pos2, tip: Tip, tips: &mut Tips) {
    egui::Area::new(egui::Id::unique("tip card"))
        .order(egui::Order::Foreground)
        .pivot(Align2::CENTER_TOP)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            theme::panel_frame().inner_margin(Margin { left: 16, right: 12, top: 12, bottom: 10 }).show(ui, |ui| {
                ui.set_width(400.0);
                ui.horizontal_top(|ui| {
                    ui.label(egui::RichText::new(crate::icons::LIGHTBULB).font(theme::icon_font(16.0)).color(ACCENT));
                    ui.add_space(2.0);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(tip.text()).color(TEXT_2));
                        ui.add_space(8.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if widgets::button(ui, "Got it", true, 30.0).clicked() {
                                tips.seen(tip);
                            }
                            if widgets::button(ui, "No more tips", false, 30.0)
                                .on_hover_text("Show tips again brings them back (in the command palette)")
                                .clicked()
                            {
                                tips.all_off();
                            }
                        });
                    });
                });
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tips_have_words_and_unique_ids() {
        for (i, a) in Tip::ALL.iter().enumerate() {
            assert!(!a.text().is_empty());
            assert!(a.text().split_whitespace().count() <= 45, "{a:?} is too long to read at a glance");
            for b in &Tip::ALL[i + 1..] {
                assert_ne!(a.id(), b.id());
            }
        }
    }
}
