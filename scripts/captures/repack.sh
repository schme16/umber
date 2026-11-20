# A model with wastefully packed UVs: paint a cross, repack in the UV workspace, show the paint
# stayed put, then undo. Records repack.mp4 and stills along the way.
# Run with target/models/crate-loose-uvs.obj from scripts/make-test-models.py.
pause 1
record repack
drag 560 420 1000 600 50
pause 0.3
drag 620 700 980 330 50
pause 0.5
shot painted
click 751 26
pause 1
shot before
click 1438 762
pause 1.5
shot after
click 696 26
pause 1.2
shot painted-after
click 751 26
pause 0.8
key ctrl+z
pause 1.2
shot undone
click 696 26
pause 1.2
shot painted-undone
stop_record
