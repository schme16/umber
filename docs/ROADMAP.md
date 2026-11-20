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
- **The bar for every feature:** Umber should feel like magic. It does the right thing
  without being asked, keeps the person's work safe through every operation, never lets the
  machinery show (no seam lines, hitches, jargon or settings in the way), and says what
  happened in plain words. A feature is done when it feels like it just worked, not when it
  works.

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
   Masks are in the data model and compositor; the brush paints them.
4. **Brush engine.** Done 2026-11-20: projection painting. The mesh is drawn in UV space and
   every texel checks itself against the dabs on screen and against a camera distance buffer,
   so strokes keep their on-screen width across UV seams and stretched UVs, never reach
   hidden surfaces, and fade on faces turned edge-on. A stroke builds up in its own buffer
   capped at its opacity, so overlapping dabs don't darken; flow sets how fast it builds.
   Size, opacity, flow, hardness, pen pressure and tilt; paints any of colour, roughness,
   metalness and height. Eraser (on a fill layer it paints the mask instead), mask tool and
   **Add mask**, fill (a UV island, or Shift for the whole material), colour and material
   picker. Each stroke is one undo step; the shader marks the 256 px tiles it touches, so only
   those are saved, and the history drops its oldest steps past 1.5 GB. Maps are padded past
   UV island edges (a jump flood finds each texel's nearest island texel), so filtering
   doesn't show seam lines. A stroke paints the material it starts on.
5. **Auto-unwrap.** Done. xatlas unwraps each material into its own UV square on a worker
   thread (with progress and Cancel); a model that opens without UVs offers it straight away,
   and painting one says why it can't. Repack keeps every island's shape and lays them out
   again. Either way the paint moves with the layout (each texel of the new layout copies
   its old position, through the old padding map so seams don't show), and the whole change
   is one undo step. The UV workspace shows UV health per material: a coverage map with
   overlaps in red, texture used, overlap, stretch, texel density, area outside 0 to 1,
   islands, and a sentence on what to do about anything that's off. Besides **Auto**, the
   **Unwrap** picker lays a model out by projection: **Planar** (from the view or an axis, with
   the back as its own island so it never shares paint with the front), **Box**, **Cylinder**
   (with flat caps) and **Sphere**. They pack through xatlas too, so each lands in the square
   without overlaps, and the paint moves with them the same way.
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
