//! Undo and redo.
//!
//! Two kinds of step: a change to a layer stack (add, delete, reorder, any setting), kept as
//! the whole stack from before (layers share their textures, so this is cheap), and a change
//! to pixels, kept as tile snapshots. Undoing a step swaps it with the current state, and the
//! swapped-out state becomes the redo step.
//!
//! A third kind, an unwrap or repack, swaps the whole mesh and every set's stack (the old
//! stacks still hold the paint in the old layout).
//!
//! Stack changes are found rather than reported: once no pointer button is held, any
//! difference from the last recorded stack becomes one step. A slider drag is one step, not
//! one per frame, and no panel code has to remember to call the history.

use eframe::egui_wgpu::wgpu;

use crate::mesh::Mesh;
use crate::texset::{same_stack, Layer, TextureSet, TileSnapshot, SIZE};

/// Oldest steps drop off past this many...
const LIMIT: usize = 200;
/// ...or once the saved pixels pass this many bytes of GPU memory.
const PIXEL_BUDGET: u64 = 1536 << 20;

pub enum Change {
    Stack { set: usize, layers: Vec<Layer>, selected: usize },
    Pixels { set: usize, tiles: TileSnapshot },
    /// The mesh and, per set, its stack and selection from before.
    Mesh { mesh: Box<Mesh>, stacks: Vec<(Vec<Layer>, usize)> },
}

impl Change {
    /// GPU memory this step keeps alive that nothing else does (roughly).
    fn bytes(&self) -> u64 {
        match self {
            Change::Pixels { tiles, .. } => tiles.bytes(),
            Change::Stack { .. } => 0,
            Change::Mesh { stacks, .. } => {
                let texels = SIZE as u64 * SIZE as u64;
                let layer = |l: &Layer| {
                    l.pixels.as_ref().map_or(0, |_| texels * (4 + 8 + 4)) + l.mask.as_ref().map_or(0, |_| texels * 4)
                };
                stacks.iter().flat_map(|(layers, _)| layers).map(layer).sum()
            }
        }
    }
}

/// What an undo or redo touched.
pub struct Applied {
    /// The texture set to show.
    pub set: usize,
    /// The mesh was swapped, so its GPU buffers and island maps need rebuilding.
    pub mesh: bool,
}

/// Every step carries an id, so the state the work is in has a name: the id of the step on
/// top of the undo stack. Saving remembers it, and undoing back to it reads as saved again.
#[derive(Default)]
pub struct History {
    undo: Vec<(u64, Change)>,
    redo: Vec<(u64, Change)>,
    /// The state with nothing left to undo.
    base: u64,
    next_id: u64,
}

impl History {
    /// Forget every step: a new model or project.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.base = self.fresh_id();
    }

    fn fresh_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Names the work as it is now. Equal states mean equal work.
    pub fn state(&self) -> u64 {
        self.undo.last().map_or(self.base, |(id, _)| *id)
    }

    /// GPU memory the undo and redo steps keep alive, roughly.
    pub fn bytes(&self) -> u64 {
        self.undo.iter().chain(&self.redo).map(|(_, c)| c.bytes()).sum()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn push(&mut self, change: Change) {
        let id = self.fresh_id();
        self.undo.push((id, change));
        self.redo.clear();
        let mut total: u64 = self.undo.iter().map(|(_, c)| c.bytes()).sum();
        while self.undo.len() > 1 && (self.undo.len() > LIMIT || total > PIXEL_BUDGET) {
            let (id, dropped) = self.undo.remove(0);
            total -= dropped.bytes();
            self.base = id;
        }
    }

    /// Record any stack that changed since the last call. Call once a frame, only while no
    /// pointer button is held.
    pub fn record_stack_changes(&mut self, sets: &mut [TextureSet]) {
        for (i, set) in sets.iter_mut().enumerate() {
            if !same_stack(&set.layers, &set.committed) {
                let before = std::mem::replace(&mut set.committed, set.layers.clone());
                self.push(Change::Stack { set: i, layers: before, selected: set.selected });
            }
        }
    }

    /// Returns the texture set the step touched, so the caller can show it.
    pub fn undo(&mut self, sets: &mut [TextureSet], mesh: &mut Mesh, device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Applied> {
        let (id, change) = self.undo.pop()?;
        let (applied, back) = apply(change, sets, mesh, device, queue);
        self.redo.push((id, back));
        Some(applied)
    }

    pub fn redo(&mut self, sets: &mut [TextureSet], mesh: &mut Mesh, device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Applied> {
        let (id, change) = self.redo.pop()?;
        let (applied, back) = apply(change, sets, mesh, device, queue);
        self.undo.push((id, back));
        Some(applied)
    }
}

fn apply(change: Change, sets: &mut [TextureSet], mesh: &mut Mesh, device: &wgpu::Device, queue: &wgpu::Queue) -> (Applied, Change) {
    let (set, back) = apply_to_sets(change, sets, mesh, device, queue);
    let swapped = matches!(back, Change::Mesh { .. });
    (Applied { set, mesh: swapped }, back)
}

fn apply_to_sets(change: Change, sets: &mut [TextureSet], mesh: &mut Mesh, device: &wgpu::Device, queue: &wgpu::Queue) -> (usize, Change) {
    match change {
        Change::Mesh { mesh: before, stacks } => {
            let current_mesh = std::mem::replace(mesh, *before);
            let current = sets
                .iter_mut()
                .zip(stacks)
                .map(|(s, (layers, selected))| {
                    let back = (std::mem::replace(&mut s.layers, layers), s.selected);
                    s.selected = selected.min(s.layers.len().saturating_sub(1));
                    s.committed = s.layers.clone();
                    s.dirty = true;
                    back
                })
                .collect();
            (0, Change::Mesh { mesh: Box::new(current_mesh), stacks: current })
        }
        Change::Stack { set, layers, selected } => {
            let s = &mut sets[set];
            let current = std::mem::replace(&mut s.layers, layers);
            let current_selected = s.selected;
            s.selected = selected.min(s.layers.len().saturating_sub(1));
            s.committed = s.layers.clone();
            (set, Change::Stack { set, layers: current, selected: current_selected })
        }
        Change::Pixels { set, tiles } => {
            let back = tiles.restore(device, queue);
            sets[set].dirty = true;
            (set, Change::Pixels { set, tiles: back })
        }
    }
}
