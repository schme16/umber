# Umber

Fast, GPU-accelerated UV unwrapping and texture/material painting for game artists.
Open a model, with or without UVs, and start painting with a pen, mouse or keyboard,
then export a game-ready model to Godot.

Status: early. Model import, the lit viewport, the GPU layer stack and the brush work; auto-unwrap and Godot export are next.
See [docs/ROADMAP.md](docs/ROADMAP.md).

## Run

```sh
cargo run --release                 # opens the welcome screen
cargo run --release -- model.glb    # opens a model straight away (binary: umber)
```

Linux only for now. You need a Vulkan driver.

## Controls

| Action | Pen / mouse | Keys |
|---|---|---|
| Orbit | Right or middle drag, or Alt + drag | |
| Pan | Shift + right or middle drag | |
| Zoom | Scroll | |
| Frame the model | | F |
| Open a model | Drop a file on the window | Ctrl+O |
| Tools | Tool rail on the left | B E S G C D M I |
| Paint | Left drag with Brush, Eraser or Mask | |
| Fill a UV island | Click with Fill (Shift + click: the whole material) | |
| Pick colour and material | Click with the Colour picker | |
| Brush size | | [ and ] |
| Swap colours | | X |
| Undo, redo | | Ctrl+Z, Ctrl+Shift+Z |
| Export to Godot | **Export to Godot** (asks for a folder the first time) | Ctrl+E |

## Formats

Opens .glb, .gltf, .obj and .fbx (binary or ASCII, read with ufbx; Z-up and centimetre files
come in Y-up and in metres).

**Export to Godot** writes a `.glb` with the painted maps inside, and beside it, for each
material, the same maps as PNGs (albedo, ORM and normal) and an `ORMMaterial3D` `.tres` that
points at them. Export into a folder inside your Godot project and Godot's importer uses those
PNGs for the model too, so there's one copy of each map. Maps are 2048, 1024 or 512 px.

## Licence

MIT. The bundled fonts are under the SIL Open Font License (Inter, JetBrains Mono)
and ISC (Lucide icons); see `assets/fonts/`.
