# Testing and captures

## The display rule

Every screenshot, recording and scripted click or key press runs on a hidden X display, never on
the desktop someone is working at. Driving the real display moves their mouse and steals focus,
and the shots come out wrong.

```bash
cargo build
scripts/capture.sh scripts/captures/pen-popover.sh path/to/model.glb
```

`capture.sh` starts Xvfb at 1600 x 1000, puts the window at 0,0, and runs the steps file with
`move`, `click`, `drag`, `key`, `pause`, `shot NAME` and `record NAME` / `stop_record` (MP4,
H.264, plays anywhere). Output goes to `target/captures/` (or `$OUT`).

Vulkan draws on the real card under Xvfb; only presenting goes through X. The log line
`GPU: NVIDIA GeForce RTX 4080 SUPER (Vulkan, DiscreteGpu)` in `target/captures/umber.log`
confirms it. If it names llvmpipe or lavapipe, the run didn't use the GPU.

Unwrapping, projecting and repacking need models with no UVs and with poorly packed ones:

```bash
python3 scripts/make-test-models.py target/models
scripts/capture.sh scripts/captures/unwrap.sh target/models/knot-no-uvs.obj
scripts/capture.sh scripts/captures/repack.sh target/models/crate-loose-uvs.obj
scripts/capture.sh scripts/captures/projections.sh target/models/vase-no-uvs.obj
```

Export needs a folder given up front, because the folder picker would open on the real desktop
through the portal. `UMBER_EXPORT_DIR` presets it:

```bash
mkdir -p target/godot-test/models   # with a project.godot in target/godot-test
UMBER_EXPORT_DIR=$PWD/target/godot-test/models scripts/capture.sh scripts/captures/export.sh
UMBER_EXPORT_DIR=$PWD/target/godot-test/models scripts/capture.sh scripts/captures/export-fbx.sh some.fbx
```

`scripts/godot-check.gd` loads everything in that project's `res://models` with a headless Godot
and prints which texture fills each material slot.

Things Xvfb cannot show (the Huion pen itself, presenting to a real monitor) go to the user as
a hand-off.

## Unit tests

`cargo test`. The pressure curve and XInput2 fixed-point maths are in `src/pen.rs`; unwrap,
the projections, repack and the UV health numbers in `src/unwrap.rs`; paint transfer to new UVs in `src/texset.rs`;
FBX import in `src/mesh.rs`; export naming, `res://` paths, map readback and a GLB round trip in
`src/export.rs`. `UMBER_TEST_MODELS=dir cargo test opens_every -- --nocapture` opens every model
in a folder and prints what came in.
