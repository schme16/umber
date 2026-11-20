# Umber

Fast, GPU-accelerated UV unwrapping and texture/material painting for game artists.
Open a model, with or without UVs, and start painting with a pen, mouse or keyboard,
then export a game-ready model to Godot.

Status: early. The shell, model import, lit viewport and GPU layer stack work; the brush is next.
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
| Brush size | | [ and ] |
| Swap colours | | X |

## Formats

Opens .glb, .gltf and .obj today. FBX import (via ufbx) is planned for milestone 1.

## Licence

MIT. The bundled fonts are under the SIL Open Font License (Inter, JetBrains Mono)
and ISC (Lucide icons); see `assets/fonts/`.
