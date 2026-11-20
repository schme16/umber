# Research notes

What was looked into while starting Umber (2026-11-20), and what it means for the build.

## The user's machine

- Huion Kamvas 13, model GS1333: 13.3" 1920×1080 pen display. The kernel exposes three
  devices: `HUION Huion Tablet_GS1333 Pen`, `… Tablet_GS1333` and `… Tablet_GS1333 Keyboard`
  (the express keys).
- X11 session (Zorin OS, GNOME), three monitors: 2× 2560×1440 and the Kamvas at 1920×1080.
- NVIDIA RTX 4080 SUPER (plus an AMD Raphael iGPU). Rust 1.97.1.
- At 150 % scaling the Kamvas gives a 1280×720 logical workspace: the tightest layout
  Umber must fit. Checked by screenshot (`images/app-paint-1280x720.png`).

## Pen input

- **winit** (used by eframe) reports no pen pressure or tilt on X11.
- **octotablet 0.1.0** (MIT) supports only Wayland `tablet-unstable-v2` and Windows Ink.
  No X11 backend, so it doesn't help on this machine.
- Plan: our own XInput2 backend through `x11rb` (MIT/Apache): find the Huion pen device,
  read its pressure and tilt valuators, and pair them with egui's pointer events.
  Windows (Windows Ink or Wintab) and Wayland come later; octotablet may cover those.

## Crates chosen (versions current on 2026-11-20)

| Crate | Version | Licence | Use |
|---|---|---|---|
| wgpu | 30.0.2 (via egui-wgpu) | MIT/Apache | GPU: Vulkan on Linux |
| eframe / egui | 0.37 | MIT/Apache | Window and UI |
| glam | 0.30 | MIT/Apache | Maths |
| gltf | 1.4 | MIT/Apache | glTF/GLB import |
| tobj | 4.0 | MIT | OBJ import |
| image | 0.25 | MIT/Apache | PNG/JPEG decode |
| rfd | 0.18 | MIT | Native file dialogs |
| xatlas-rs | 0.1 (planned) | MIT | Auto-unwrap and packing |
| ufbx | 0.11 (planned) | MIT bindings around MIT/public-domain C | FBX import |
| egui_dock | 0.21 (maybe) | MIT | Docking, if ever needed |

## API notes for this stack (they changed recently)

- eframe 0.37: `App::ui(&mut self, ui, frame)` replaces `update`; `NativeOptions` has no
  `vsync` field any more.
- egui 0.37: `egui::Panel::top/left/…` (takes the root `Ui`); `Id::new` is deprecated in favour
  of `Id::unique`; `Ui::id` is now `Ui::scope_id`; text measuring is `ctx.fonts_mut`;
  styles are set with `ctx.all_styles_mut`; `Shadow` uses `i8`/`u8` fields.
- wgpu 30: `vertex.buffers` and `bind_group_layouts` take `Option`s; pipelines have
  `multiview_mask`; colour attachments have `depth_slice`; `depth_write_enabled` and
  `depth_compare` are `Option`s; mipmap filter type is `MipmapFilterMode`.
- **Gamma pitfall:** egui-wgpu shows a registered native texture's bytes as they are, with no
  sRGB-to-linear conversion. Rendering into an `Rgba8UnormSrgb` texture and handing that view
  to egui made the viewport far too dark ((3,3,3) instead of (26,27,30)). Fix: an `Rgba8Unorm`
  texture with an `Rgba8UnormSrgb` view format; render through the sRGB view, give egui the
  UNORM view.
- egui can't wrap nested `ui.horizontal` groups inside `horizontal_wrapped`, so the brush
  bar switches between one and two rows by width.

## Licensing constraints

The user wants MIT, private for now, so only permissive code may ship:

- **No GPL**: Blender's unwrapper and similar GPL code can't be copied or linked.
  LSCM/ABF are published algorithms, so we implement them ourselves.
- **No Autodesk FBX SDK**: its licence doesn't allow this kind of redistribution. ufbx
  covers FBX import; FBX export is a later, separate question (glTF/GLB is the fallback the
  user accepted).
- Fonts: Inter and JetBrains Mono (SIL OFL 1.1), Lucide icons (ISC). Licence files are in
  `assets/fonts/`.

## Export targets

- **Godot 4** (required): imports glTF/GLB natively, including PBR materials. Plan: GLB with
  embedded maps, plus a `.tres` StandardMaterial3D using ORM-packed textures (occlusion R,
  roughness G, metallic B), which is what Godot's ORM material expects. The material graph
  can later export a live `.gdshader`, the way Material Maker does.
- **Unity** (nice to have): Metallic-Smoothness packing (metal in R, smoothness in A),
  HDRP mask map (metal R, AO G, detail B, smoothness A).
- **Unreal** (nice to have): ORM packing; normal maps need the green channel flipped
  (DirectX convention).

## Reference apps

The node graph aims at Material Maker (Godot-based) and Substance Designer; layer painting
at Substance Painter and ArmorPaint. The rule is to match what they can do, not copy their
look or names.
