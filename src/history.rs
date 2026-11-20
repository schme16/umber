//! Undo and redo.
//!
//! Two kinds of step: a change to a layer stack (add, delete, reorder, any setting), kept as
//! the whole stack from before (layers share their textures, so this is cheap), and a change
//! to pixels, kept as tile snapshots. Undoing a step swaps it with the current state, and the
//! swapped-out state becomes the redo step.
//!
//! Stack changes are found rather than reported: once no pointer button is held, any
//! difference from the last recorded stack becomes one step. A slider drag is one step, not
//! one per frame, and no panel code has to remember to call the history.

use eframe::egui_wgpu::wgpu;

use crate::texset::{same_stack, Layer, TextureSet, TileSnapshot};

/// Oldest steps drop off past this many...
const LIMIT: usize = 200;
/// ...or once the saved pixels pass this many bytes of GPU memory.
const PIXEL_BUDGET: u64 = 1536 << 20;

pub enum Change {
    Stack { set: usize, layers: Vec<Layer>, selected: usize },
    Pixels { set: usize, tiles: TileSnapshot },
}

#[derive(Default)]
pub struct History {
    undo: Vec<Change>,
    redo: Vec<Change>,
}

impl History {
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn push(&mut self, change: Change) {
        self.undo.push(change);
        self.redo.clear();
        let bytes = |c: &Change| match c {
            Change::Pixels { tiles, .. } => tiles.bytes(),
            Change::Stack { .. } => 0,
        };
        let mut total: u64 = self.undo.iter().map(bytes).sum();
        while self.undo.len() > 1 && (self.undo.len() > LIMIT || total > PIXEL_BUDGET) {
            total -= bytes(&self.undo.remove(0));
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
    pub fn undo(&mut self, sets: &mut [TextureSet], device: &wgpu::Device, queue: &wgpu::Queue) -> Option<usize> {
        let change = self.undo.pop()?;
        let (set, back) = apply(change, sets, device, queue);
        self.redo.push(back);
        Some(set)
    }

    pub fn redo(&mut self, sets: &mut [TextureSet], device: &wgpu::Device, queue: &wgpu::Queue) -> Option<usize> {
        let change = self.redo.pop()?;
        let (set, back) = apply(change, sets, device, queue);
        self.undo.push(back);
        Some(set)
    }
}

fn apply(change: Change, sets: &mut [TextureSet], device: &wgpu::Device, queue: &wgpu::Queue) -> (usize, Change) {
    match change {
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
