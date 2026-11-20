//! Every command Umber has, declared once. Keys, tooltips, the command palette and the
//! shortcut sheet all read from [`COMMANDS`], so the key a tooltip names is always the key
//! that works. Whether a command can run right now, and why not, lives in the app
//! (`UmberApp::unavailable`), since it depends on the open work.

use egui::Key;

use crate::app::{Tool, UnwrapMethod, Workspace};
use crate::camera::{Pivot, Turning};
use crate::renderer::ViewMode;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    Open,
    Save,
    SaveAs,
    Export,
    ExportFolder,
    Quit,
    Undo,
    Redo,
    Tool(Tool),
    BrushSmaller,
    BrushBigger,
    SwapColours,
    ColourPanel,
    BrushTexture,
    OpenBrushTexture,
    ReachAll,
    ReachFace,
    PenSettings,
    PaintColour,
    PaintRough,
    PaintMetal,
    PaintHeight,
    NewPaintLayer,
    NewFillLayer,
    AddMask,
    DuplicateLayer,
    DeleteLayer,
    LayerUp,
    LayerDown,
    Frame,
    CameraSettings,
    TurnAbout(Pivot),
    Turning(Turning),
    ZoomToCursor,
    SpinOnFlick,
    View(ViewMode),
    Workspace(Workspace),
    Unwrap(UnwrapMethod),
    Repack,
    Stats,
    Palette,
    Shortcuts,
    ResetTips,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    File,
    Edit,
    Tools,
    Brush,
    Layers,
    View,
    Uv,
    Help,
}

impl Group {
    pub const ALL: [Group; 8] = [Group::File, Group::Edit, Group::Tools, Group::Brush, Group::Layers, Group::View, Group::Uv, Group::Help];

    pub fn name(self) -> &'static str {
        match self {
            Group::File => "File",
            Group::Edit => "Edit",
            Group::Tools => "Tools",
            Group::Brush => "Brush",
            Group::Layers => "Layers",
            Group::View => "View",
            Group::Uv => "UVs",
            Group::Help => "Help",
        }
    }
}

/// A key with Ctrl and/or Shift held. Keys that already need Shift to type (like `?`) leave
/// `shift` false: the key itself says it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shortcut {
    pub ctrl: bool,
    pub shift: bool,
    pub key: Key,
}

const fn key(key: Key) -> Shortcut {
    Shortcut { ctrl: false, shift: false, key }
}
const fn ctrl(key: Key) -> Shortcut {
    Shortcut { ctrl: true, shift: false, key }
}
const fn ctrl_shift(key: Key) -> Shortcut {
    Shortcut { ctrl: true, shift: true, key }
}

impl Shortcut {
    /// Keys that are typed with Shift, so Shift is part of the key rather than a modifier.
    fn shifted_symbol(self) -> bool {
        matches!(self.key, Key::Questionmark | Key::Plus | Key::Colon | Key::Pipe)
    }

    /// Whether a key press with these modifiers is this shortcut.
    pub fn matches(self, key: Key, m: egui::Modifiers) -> bool {
        key == self.key && m.command == self.ctrl && !m.alt && (self.shifted_symbol() || m.shift == self.shift)
    }

    /// The way it's written in the app: "Ctrl+Shift+S", "[", "F1".
    pub fn label(self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(self.key.symbol_or_name());
        s
    }

    /// Each key on its own, for drawing keycaps.
    pub fn caps(self) -> Vec<String> {
        let mut caps = Vec::new();
        if self.ctrl {
            caps.push("Ctrl".to_owned());
        }
        if self.shift {
            caps.push("Shift".to_owned());
        }
        caps.push(self.key.symbol_or_name().to_owned());
        caps
    }
}

pub struct Command {
    pub cmd: Cmd,
    pub name: &'static str,
    pub group: Group,
    /// The first is the one shown; the rest also work.
    pub keys: &'static [Shortcut],
    /// Other words people use for it, space separated. Searched, never shown.
    pub words: &'static str,
    /// One plain sentence: what it does.
    pub about: &'static str,
}

const fn c(cmd: Cmd, name: &'static str, group: Group, keys: &'static [Shortcut], words: &'static str, about: &'static str) -> Command {
    Command { cmd, name, group, keys, words, about }
}

use Group as G;

pub const COMMANDS: &[Command] = &[
    // File
    c(Cmd::Open, "Open…", G::File, &[ctrl(Key::O)], "load import model project file fbx gltf glb obj mesh", "Open a model (glTF, OBJ, FBX) or an .umber project"),
    c(Cmd::Save, "Save", G::File, &[ctrl(Key::S)], "keep store write project umber", "Save the model, every layer and your settings to one .umber file"),
    c(Cmd::SaveAs, "Save as…", G::File, &[ctrl_shift(Key::S)], "copy rename new project umber", "Save to a new .umber file, and keep working on that one"),
    c(Cmd::Export, "Export to Godot", G::File, &[ctrl(Key::E)], "glb gltf textures maps output engine game send write", "Write a .glb, a Godot material and its texture maps"),
    c(Cmd::ExportFolder, "Choose export folder…", G::File, &[], "destination location directory where godot export", "Pick where Export to Godot writes its files"),
    c(Cmd::Quit, "Quit", G::File, &[ctrl(Key::Q)], "exit close leave", "Close Umber, asking first about unsaved work"),
    // Edit
    c(Cmd::Undo, "Undo", G::Edit, &[ctrl(Key::Z)], "back revert oops mistake step", "Take back the last change"),
    c(Cmd::Redo, "Redo", G::Edit, &[ctrl_shift(Key::Z), ctrl(Key::Y)], "again forward repeat", "Put back what Undo took away"),
    // Tools
    c(Cmd::Tool(Tool::Brush), "Brush", G::Tools, &[key(Key::B)], "paint draw pen stroke", "Paint colour and material onto the model"),
    c(Cmd::Tool(Tool::Eraser), "Eraser", G::Tools, &[key(Key::E)], "erase rubber remove clear wipe", "Rub out paint on this layer (on a fill layer it paints the mask)"),
    c(Cmd::Tool(Tool::Smudge), "Smudge", G::Tools, &[key(Key::S)], "blur smear blend finger", "Smear paint around. Comes later, with smart layers"),
    c(Cmd::Tool(Tool::Fill), "Fill", G::Tools, &[key(Key::G)], "bucket paint flood island", "Fill a UV island with one click, or the whole material with Shift"),
    c(Cmd::Tool(Tool::Clone), "Clone stamp", G::Tools, &[key(Key::C)], "stamp copy", "Copy paint from one place to another. Comes later, with smart layers"),
    c(Cmd::Tool(Tool::Decal), "Decal", G::Tools, &[key(Key::D)], "sticker stencil logo image", "Place an image on the model. Comes later, with smart layers"),
    c(Cmd::Tool(Tool::Mask), "Mask", G::Tools, &[key(Key::M)], "hide reveal stencil", "Paint where a layer shows: dark hides it, light shows it"),
    c(Cmd::Tool(Tool::Picker), "Colour picker", G::Tools, &[key(Key::I)], "eyedropper dropper sample pick color", "Click the model to pick up its colour and material"),
    // Brush
    c(Cmd::BrushSmaller, "Smaller brush", G::Brush, &[key(Key::OpenBracket)], "decrease shrink reduce size tiny", "Make the brush a little smaller"),
    c(Cmd::BrushBigger, "Bigger brush", G::Brush, &[key(Key::CloseBracket)], "increase grow larger size huge", "Make the brush a little bigger"),
    c(Cmd::SwapColours, "Swap colours", G::Brush, &[key(Key::X)], "switch exchange primary secondary color foreground background", "Swap the front and back colours"),
    c(Cmd::ColourPanel, "Brush colour…", G::Brush, &[], "color pick choose hue palette wheel roughness metalness", "Choose the colour and material values the brush paints"),
    c(Cmd::BrushTexture, "Paint with a texture…", G::Brush, &[], "image picture photo pattern stamp stencil bricks wood planks tiles grunge dirt triplanar", "Paint a pattern or an image instead of one colour"),
    c(Cmd::OpenBrushTexture, "Open texture image…", G::Brush, &[], "load png jpg jpeg photo picture file stamp stencil import", "Pick an image file for the brush to paint with"),
    c(Cmd::ReachAll, "Paint everything under the brush", G::Brush, &[], "reach all faces sides wrap around round over corners edges geometry", "Strokes reach every surface the brush covers, round corners too"),
    c(Cmd::ReachFace, "Paint one face only", G::Brush, &[], "reach face side surface contiguous stay inside edge edges corner corners stop going round bleed geometry limit restrict", "Strokes keep to the face they start on, stopping at sharp edges"),
    c(Cmd::PenSettings, "Pen pressure…", G::Brush, &[], "tablet stylus huion wacom curve sensitivity softness tilt", "Tune how pen pressure maps to the brush"),
    c(Cmd::PaintColour, "Paint colour", G::Brush, &[], "channel albedo base color", "Whether the brush and fill paint base colour"),
    c(Cmd::PaintRough, "Paint roughness", G::Brush, &[], "channel rough shiny glossy matte", "Whether the brush and fill paint roughness"),
    c(Cmd::PaintMetal, "Paint metalness", G::Brush, &[], "channel metal metallic", "Whether the brush and fill paint metalness"),
    c(Cmd::PaintHeight, "Paint height", G::Brush, &[], "channel bump bumps displacement normal", "Whether the brush and fill paint height (bumps)"),
    // Layers
    c(Cmd::NewPaintLayer, "New paint layer", G::Layers, &[], "add create blank empty", "Add an empty layer above the selected one, for the brush"),
    c(Cmd::NewFillLayer, "New fill layer", G::Layers, &[], "add create solid base", "Add a layer of one colour and material everywhere"),
    c(Cmd::AddMask, "Add mask", G::Layers, &[], "layer hide stencil", "Give the selected layer a mask, then paint it with the Mask tool"),
    c(Cmd::DuplicateLayer, "Duplicate layer", G::Layers, &[], "copy clone", "Copy the selected layer"),
    c(Cmd::DeleteLayer, "Delete layer", G::Layers, &[], "remove trash bin", "Remove the selected layer"),
    c(Cmd::LayerUp, "Move layer up", G::Layers, &[], "raise order above forward", "Move the selected layer above the one over it"),
    c(Cmd::LayerDown, "Move layer down", G::Layers, &[], "lower order below backward", "Move the selected layer below the one under it"),
    // View
    c(Cmd::Frame, "Frame the model", G::View, &[key(Key::F)], "fit zoom focus center centre reset camera home", "Bring the whole model into view"),
    c(Cmd::CameraSettings, "Camera settings…", G::View, &[], "navigation navigate turn rotate orbit pivot zoom pan move model view options trackball turntable momentum", "Choose how dragging turns the model and how scrolling zooms"),
    c(Cmd::TurnAbout(Pivot::Cursor), "Turn about what you grab", G::View, &[], "rotate orbit pivot point under cursor mouse pick natural hold object camera", "The point under the cursor stays put while the model turns about it"),
    c(Cmd::TurnAbout(Pivot::Centre), "Turn about the model's centre", G::View, &[], "rotate orbit pivot center centre middle classic camera", "The model always turns about its own middle"),
    c(Cmd::Turning(Turning::Turntable), "Turning keeps up up", G::View, &[], "turntable rotate orbit level upright horizon camera", "Sideways drags turn about the vertical, so the model never tips over"),
    c(Cmd::Turning(Turning::Free), "Free turning", G::View, &[], "trackball arcball tumble roll rotate orbit any direction upside down camera", "The model rolls whichever way you drag, like a ball under your hand"),
    c(Cmd::ZoomToCursor, "Zoom towards the cursor", G::View, &[], "scroll wheel dolly zoom mouse point camera", "Scrolling zooms towards what's under the cursor, not the middle of the view"),
    c(Cmd::SpinOnFlick, "Keep turning after a flick", G::View, &[], "momentum inertia spin spinning stop throw coast ease glide camera", "Let go mid-drag and the model turns on for a moment, then settles"),
    c(Cmd::View(ViewMode::Lit), "Lit view", G::View, &[], "lighting shaded render pbr material", "See the model lit, as it will look in the game"),
    c(Cmd::View(ViewMode::Colour), "Colour view", G::View, &[], "albedo base unlit flat diffuse color", "See only the base colour, without lighting"),
    c(Cmd::View(ViewMode::UvCheck), "UV checker view", G::View, &[], "checker grid stretch distortion", "Cover the model in a checker to show how its UVs stretch"),
    c(Cmd::View(ViewMode::Normals), "Normals view", G::View, &[], "normal bumps", "Show which way each part of the surface faces"),
    c(Cmd::Stats, "Frame and GPU stats", G::View, &[key(Key::F3)], "fps performance profiler frame time speed slow lag debug overlay hud gpu cpu memory vram triangles draw calls", "Show how long each frame takes, and where the time and memory go"),
    c(Cmd::Workspace(Workspace::Paint), "Paint workspace", G::View, &[], "painting layers", "The brush, layers and colour"),
    c(Cmd::Workspace(Workspace::Uv), "UV workspace", G::View, &[], "uvs layout health islands", "Check and redo how the model lies on its texture"),
    c(Cmd::Workspace(Workspace::Materials), "Materials workspace", G::View, &[], "graph nodes shader", "The material graph, which comes later"),
    c(Cmd::Workspace(Workspace::Export), "Export workspace", G::View, &[], "output godot files size", "Where exports go, at what size, and every file written"),
    // UVs
    c(Cmd::Unwrap(UnwrapMethod::Auto), "Auto-unwrap", G::Uv, &[], "uv unwrap layout seams flatten atlas", "Cut seams where they show least and lay the pieces out"),
    c(Cmd::Repack, "Repack UVs", G::Uv, &[], "pack islands tighter space layout", "Keep every island's shape and lay them out tighter"),
    c(Cmd::Unwrap(UnwrapMethod::Planar), "Planar unwrap", G::Uv, &[], "uv flat projection project front", "Lay the model out flat as the camera sees it now"),
    c(Cmd::Unwrap(UnwrapMethod::Box), "Box unwrap", G::Uv, &[], "uv cube triplanar projection hard", "Each face flat from the side it faces most"),
    c(Cmd::Unwrap(UnwrapMethod::Cylinder), "Cylinder unwrap", G::Uv, &[], "uv tube wrap limb bottle projection", "Wrap the model around an axis and unroll it"),
    c(Cmd::Unwrap(UnwrapMethod::Sphere), "Sphere unwrap", G::Uv, &[], "uv globe ball head round projection", "Wrap the model like a globe"),
    // Help
    c(Cmd::Palette, "Search commands", G::Help, &[ctrl(Key::K)], "find command palette actions everything menu", "Find any command by typing what you want"),
    c(Cmd::Shortcuts, "Keyboard shortcuts", G::Help, &[key(Key::Questionmark), key(Key::F1)], "keys hotkeys keybindings help cheat sheet rotate orbit turn pan navigate mouse pen", "Every key Umber knows, on one sheet"),
    c(Cmd::ResetTips, "Show tips again", G::Help, &[], "hints help tutorial onboarding reset first", "Bring back the tips that show the first time you use something"),
];

pub fn get(cmd: Cmd) -> &'static Command {
    COMMANDS.iter().find(|c| c.cmd == cmd).expect("every command is in the registry")
}

/// The main shortcut, written the way the app shows it.
pub fn key_label(cmd: Cmd) -> Option<String> {
    get(cmd).keys.first().map(|k| k.label())
}

/// "Save (Ctrl+S)", for tooltips.
pub fn tip(cmd: Cmd) -> String {
    let c = get(cmd);
    match key_label(cmd) {
        Some(k) => format!("{} ({k})", c.name.trim_end_matches('…')),
        None => c.name.trim_end_matches('…').to_owned(),
    }
}

// ---------------------------------------------------------------- search

/// Words that say nothing about which command is meant.
const FILLER: &[&str] = &[
    "a", "an", "the", "i", "me", "my", "to", "do", "how", "can", "it", "this", "that", "of", "on", "in", "for", "with", "is", "want", "please",
    "some", "make", "get", "go", "use",
];

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

/// British and American spellings both find the same thing.
fn normalise(w: &str) -> String {
    w.replace("color", "colour")
}

/// Edit distance, with two swapped neighbours counting as one slip, for forgiving typos.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

/// How well one query word fits one word of a command. 0 is no fit.
fn fit(q: &str, w: &str) -> u32 {
    if q == w {
        return 10;
    }
    if w.starts_with(q) {
        return if q.len() >= 2 { 8 } else { 3 };
    }
    let n = q.chars().count();
    if n >= 4 {
        let allowed = if n >= 7 { 2 } else { 1 };
        // A typo in the whole word, or in the start of a longer one ("exprt" for "exporting").
        let head: String = w.chars().take(n).collect();
        if distance(q, w) <= allowed || (n >= 5 && w.chars().count() > n && distance(q, &head) <= allowed) {
            return 4;
        }
    }
    if n >= 3 && w.contains(q) {
        return 3;
    }
    0
}

/// How well `query` fits a command: 0 is not at all. Words in the name count most, then
/// the other words, then the group.
pub fn score(command: &Command, query: &str) -> u32 {
    let mut q: Vec<String> = words(query).iter().map(|w| normalise(w)).collect();
    if q.iter().any(|w| !FILLER.contains(&w.as_str())) {
        q.retain(|w| !FILLER.contains(&w.as_str()));
    }
    if q.is_empty() {
        return 1;
    }
    let name: Vec<String> = words(command.name).iter().map(|w| normalise(w)).collect();
    let other: Vec<String> = words(command.words).iter().map(|w| normalise(w)).collect();
    let group = normalise(&command.group.name().to_lowercase());
    let mut total = 0;
    let mut missed = 0;
    let mut exact = false;
    for w in &q {
        let best = name
            .iter()
            .map(|n| fit(w, n) * 3)
            .chain(other.iter().map(|o| fit(w, o) * 2))
            .chain(std::iter::once(fit(w, &group)))
            .max()
            .unwrap_or(0);
        if best == 0 {
            missed += 1;
        }
        exact |= best >= 20;
        total += best;
    }
    // Every word should land somewhere. One stray word is forgiven in a sentence, or beside a
    // word that names the command outright ("save my work").
    if missed > 0 && (missed > 1 || missed == q.len() || !(q.len() >= 3 || exact)) {
        return 0;
    }
    // Anything that fits every word ranks above what fits only some.
    if missed > 0 {
        total /= 2;
    }
    // The whole query at the start of the name beats the same words scattered about.
    let lower = normalise(&command.name.to_lowercase());
    if lower.starts_with(&q.join(" ")) {
        total += 6;
    }
    total
}

/// Commands that fit `query`, best first.
pub fn search(query: &str) -> Vec<Cmd> {
    let mut hits: Vec<(u32, usize)> =
        COMMANDS.iter().enumerate().filter_map(|(i, c)| Some((score(c, query), i)).filter(|(s, _)| *s > 0)).collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    // Far weaker fits than the best are noise ("redo" finding "reduce").
    let best = hits.first().map_or(0, |h| h.0);
    hits.retain(|h| h.0 * 3 >= best);
    hits.into_iter().map(|(_, i)| COMMANDS[i].cmd).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Things people actually type, and what each must find in the top three. Add a row every
    /// time somebody can't find something.
    const PHRASES: &[(&str, Cmd)] = &[
        ("save", Cmd::Save),
        ("save my work", Cmd::Save),
        ("save as", Cmd::SaveAs),
        ("open fbx", Cmd::Open),
        ("load a model", Cmd::Open),
        ("export", Cmd::Export),
        ("exprot", Cmd::Export),
        ("send to godot", Cmd::Export),
        ("where does export go", Cmd::ExportFolder),
        ("undo", Cmd::Undo),
        ("oops", Cmd::Undo),
        ("redo", Cmd::Redo),
        ("rotate the model", Cmd::CameraSettings),
        ("orbit", Cmd::CameraSettings),
        ("orbit around the center", Cmd::TurnAbout(Pivot::Centre)),
        ("rotate around pivot", Cmd::TurnAbout(Pivot::Cursor)),
        ("trackball", Cmd::Turning(Turning::Free)),
        ("turntable", Cmd::Turning(Turning::Turntable)),
        ("camera options", Cmd::CameraSettings),
        ("stop the model spinning", Cmd::SpinOnFlick),
        ("rubber", Cmd::Tool(Tool::Eraser)),
        ("eyedropper", Cmd::Tool(Tool::Picker)),
        ("paint bucket", Cmd::Tool(Tool::Fill)),
        ("bigger brush", Cmd::BrushBigger),
        ("make the brush smaller", Cmd::BrushSmaller),
        ("brush size", Cmd::BrushBigger),
        ("swap colors", Cmd::SwapColours),
        ("pick a color", Cmd::ColourPanel),
        ("paint with a picture", Cmd::BrushTexture),
        ("brick pattern", Cmd::BrushTexture),
        ("stencil", Cmd::BrushTexture),
        ("load an image to paint", Cmd::OpenBrushTexture),
        ("only paint one side", Cmd::ReachFace),
        ("stop paint going round corners", Cmd::ReachFace),
        ("paint over edges", Cmd::ReachAll),
        ("pressure", Cmd::PenSettings),
        ("tablet", Cmd::PenSettings),
        ("paint roughness", Cmd::PaintRough),
        ("metallic", Cmd::PaintMetal),
        ("bumps", Cmd::PaintHeight),
        ("new layer", Cmd::NewPaintLayer),
        ("add layer", Cmd::NewPaintLayer),
        ("solid colour layer", Cmd::NewFillLayer),
        ("mask", Cmd::AddMask),
        ("copy layer", Cmd::DuplicateLayer),
        ("delete layer", Cmd::DeleteLayer),
        ("zoom to fit", Cmd::Frame),
        ("reset camera", Cmd::Frame),
        ("checker", Cmd::View(ViewMode::UvCheck)),
        ("unlit", Cmd::View(ViewMode::Colour)),
        ("unwrap", Cmd::Unwrap(UnwrapMethod::Auto)),
        ("how do i unwrap it", Cmd::Unwrap(UnwrapMethod::Auto)),
        ("uv", Cmd::Workspace(Workspace::Uv)),
        ("pack islands", Cmd::Repack),
        ("cylinder", Cmd::Unwrap(UnwrapMethod::Cylinder)),
        ("hotkeys", Cmd::Shortcuts),
        ("keyboard", Cmd::Shortcuts),
        ("tips", Cmd::ResetTips),
        ("quit", Cmd::Quit),
        ("exit", Cmd::Quit),
    ];

    #[test]
    fn phrases_find_their_command() {
        let mut failures = Vec::new();
        for (phrase, want) in PHRASES {
            let found = search(phrase);
            if !found.iter().take(3).any(|c| c == want) {
                let top: Vec<&str> = found.iter().take(3).map(|c| get(*c).name).collect();
                failures.push(format!("{phrase:?} wants {:?}, top three were {top:?}", get(*want).name));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn nonsense_finds_nothing() {
        assert!(search("zebra crossing").is_empty());
    }

    #[test]
    fn registry_is_sound() {
        for (i, a) in COMMANDS.iter().enumerate() {
            assert!(!a.name.is_empty() && !a.about.is_empty() && !a.words.is_empty(), "{} needs a name, words and about", a.name);
            for b in &COMMANDS[i + 1..] {
                assert!(a.cmd != b.cmd, "{} is in the registry twice", a.name);
                assert!(a.name != b.name, "two commands are called {}", a.name);
                for k in a.keys {
                    assert!(!b.keys.contains(k), "{} and {} both use {}", a.name, b.name, k.label());
                }
            }
        }
        for tool in Tool::ALL {
            get(Cmd::Tool(tool));
        }
        for (method, _) in UnwrapMethod::ALL {
            get(Cmd::Unwrap(method));
        }
    }

    /// Prose that names a shortcut must name one that exists, so help can't drift from the keys.
    #[test]
    fn prose_names_only_real_shortcuts() {
        let labels: Vec<String> = COMMANDS.iter().flat_map(|c| c.keys.iter().map(|k| k.label())).collect();
        let sources = [("app.rs", include_str!("app.rs")), ("help.rs", include_str!("help.rs")), ("commands.rs", include_str!("commands.rs"))];
        let mut bad = Vec::new();
        for (file, src) in sources {
            for (at, _) in src.match_indices("Ctrl+") {
                let rest = &src[at..];
                let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '+')).unwrap_or(rest.len());
                let named = &rest[..end];
                // "Ctrl+" on its own is this test or the label builder.
                if named == "Ctrl+" || named.ends_with('+') {
                    continue;
                }
                if !labels.iter().any(|l| l == named) {
                    bad.push(format!("{file} names {named}, which no command uses"));
                }
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}
