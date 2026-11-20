# Umber: roadmap and architecture

## Decisions (2026-11-20)

- **Name:** Umber (confirmed as the final name 2026-11-20). Umber is the warm earth pigment beside the brass accent.
- **Look:** Direction A "Graphite" from the mood boards
  (https://claude.ai/artifact/E8GbWYxUjGiBd64yMBB9PH). Dark ground, full-bleed viewport,
  floating rounded panels, one accent. Colours are the "Umber and brass" palette (chosen
  2026-11-20, see docs/THEME.md); the viewport stays neutral grey. Panels collapse to edge strips on small
  pen displays; a radial tool menu sits on the pen's side button.
- **Platforms:** Linux first. Windows and macOS later.
- **Licence:** MIT, private for now. Only permissive dependencies (MIT, Apache, BSD,
  zlib, ISC, OFL fonts). No GPL code, no Autodesk FBX SDK.
- **Must-have export:** Godot 4. Unity and Unreal are nice to have.

## Stack

| Concern | Choice | Why |
|---|---|---|
| Language | Rust | Speed, safety, one binary |
| GPU | wgpu 30 (Vulkan on Linux) | Portable to DX12/Metal later; compute shaders for painting |
| UI | egui 0.37 via eframe, custom-painted Graphite widgets | Immediate mode suits a tool app; full control of look |
| Pen input | Own XInput2 backend (x11rb) | winit and octotablet give no pressure on X11 |
| Import | gltf, tobj, ufbx (planned) | Permissive, cover glTF/GLB, OBJ, FBX |
| Unwrap | xatlas (MIT) for auto-unwrap and packing; own LSCM for seam-based unwrap | xatlas is proven; LSCM gives seam control |
| Export | glTF/GLB writer + Godot `.tres` StandardMaterial3D | Godot imports glTF natively |

## Code map

- `src/app.rs`: app state and the Graphite shell (top bar, viewport, floating panels)
- `src/renderer.rs` + `src/shaders/viewport.wgsl`: offscreen MSAA viewport, GGX lighting, view modes
- `src/mesh.rs`: mesh data, glTF/OBJ import, built-in shapes
- `src/camera.rs`: orbit camera
- `src/theme.rs`, `src/widgets.rs`, `src/icons.rs`: the Graphite design system
- `src/pen.rs`: XInput2 pen pressure/tilt thread (raw events on the root window) and the pressure curve
- `src/texset.rs` + `src/shaders/composite.wgsl`: texture sets, the layer model, the compute
  compositor (one blend pass per layer, then resolve to colour / material / normal maps with
  mips), and tiled undo snapshots
- `src/history.rs`: undo and redo (whole-stack snapshots for layer edits, tiles for pixels)
- `scripts/capture.sh`: screenshots and videos on a hidden Xvfb display (see docs/TESTING.md)

## Milestone 1: the paint loop end to end

Open a model, auto-unwrap if it has no UVs, paint colour/roughness/metal/height layers
with the pen, see it lit, export to Godot.

1. **Shell and viewport.** Done: window, Graphite theme, workspaces, floating panels,
   glTF/OBJ import with transforms and base-colour textures, orbit camera, lit/colour/
   UV-checker/normals views, drag-and-drop, welcome screen.
2. **Pen input.** Done (pending a hands-on check with the pen): XInput2 raw events give
   pressure, tilt and contact for any tablet with an "Abs Pressure" valuator; a mouse moving
   hands control back. Pressure curve (softness, lightest touch) in the brush bar; the brush
   cursor shows pressure size and tilt.
3. **Texture sets and layer stack on the GPU.** Done 2026-11-20: one 2048 px texture set per
   material, seeded from the file (a "Base material" fill layer, the base colour texture as a
   paint layer). Paint layers keep colour, material values and per-channel coverage, so a
   stroke can paint colour and roughness and leave metal alone; fill layers set constant
   values per channel. Nine blend modes, opacity, visibility, reorder, duplicate, delete.
   Compute shaders flatten the stack into colour, material (rough/metal/height/AO) and a
   height-derived normal map, only when something changed. Undo/redo covers every layer edit
   (a slider drag is one step); tiled pixel snapshots are built and tested, ready for strokes.
   Masks are in the data model and compositor; their button arrives with the brush, which is
   what paints them.
4. **Brush engine.** Project brush dabs from screen to UV space via a GPU position/ID
   buffer; spacing, pressure, flow, hardness, seam-aware bleeding; eraser, fill, picker.
5. **Auto-unwrap.** xatlas for models without UVs, plus the UV health readout
   (stretch, overlaps, space used, texel density).
6. **Export.** GLB with embedded PBR maps, plus a Godot `.tres` StandardMaterial3D
   pointing at ORM-packed PNGs. FBX import via ufbx.

## Later milestones

- UV workspace: seams, LSCM unwrap from seams, packing, straighten, pin, stretch heatmap.
- Smart layers: curvature/AO/edge generators, fill layers from materials, decals,
  clone stamp, smudge, symmetry.
- Material graph: node-based PBR on GPU compute; bake to maps, or export a live
  `.gdshader` for Godot.
- Pattern and texture generators (bricks, planks, tiles, noise, rust, grime).
- Tablet layout: radial menu, collapsible edge panels, UI scale presets for 8–16" displays.
- Unity and Unreal export presets (channel packing, normal map Y flip).
- Windows (Windows Ink / Wintab) and macOS.
