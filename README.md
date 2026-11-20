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

## Formats

Opens .glb, .gltf and .obj today. FBX import (via ufbx) is planned for milestone 1.

## Licence

MIT. The bundled fonts are under the SIL Open Font License (Inter, JetBrains Mono)
and ISC (Lucide icons); see `assets/fonts/`.
