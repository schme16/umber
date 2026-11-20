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

Unwrapping and repacking need models with no UVs and with poorly packed ones:

```bash
python3 scripts/make-test-models.py target/models
scripts/capture.sh scripts/captures/unwrap.sh target/models/knot-no-uvs.obj
scripts/capture.sh scripts/captures/repack.sh target/models/crate-loose-uvs.obj
```

Things Xvfb cannot show (the Huion pen itself, presenting to a real monitor) go to the user as
a hand-off.

## Unit tests

`cargo test`. The pressure curve and XInput2 fixed-point maths are in `src/pen.rs`; unwrap,
repack and the UV health numbers in `src/unwrap.rs`; paint transfer to new UVs in `src/texset.rs`.
