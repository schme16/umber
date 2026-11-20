# A model without UVs: the offer card, Auto-unwrap with its progress, then the UV workspace.
# Run with target/models/knot-no-uvs.obj from scripts/make-test-models.py. Records unwrap.mp4; stills offer, progress, unwrapped, uv-health.
pause 1
record unwrap
pause 1
shot offer
click 537 297
pause 0.2
shot progress
pause 2.5
shot unwrapped
pause 0.5
click 751 26
pause 1.5
shot uv-health
stop_record
