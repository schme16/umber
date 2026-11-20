# The unwrap methods on one model without UVs: the offer card with More ways, then the UV
# workspace with each method in turn, undone between them so the panel stays put. Run with
# target/models/vase-no-uvs.obj from scripts/make-test-models.py at 1600x1000.
# Stills: offer, planar-picker, then proj-planar, proj-box, proj-cylinder, proj-sphere, proj-auto.
pause 1.5
shot offer
click 653 280
pause 1
click 1355 782
pause 0.5
shot planar-picker
# Planar from the view
click 1317 917
pause 3
shot proj-planar
key ctrl+z
pause 1
# Box: no axis row, so Unwrap sits higher
click 1405 782
pause 0.5
click 1317 885
pause 3
shot proj-box
key ctrl+z
pause 1
# Cylinder around Y
click 1461 782
pause 0.5
click 1317 917
pause 3
shot proj-cylinder
key ctrl+z
pause 1
# Sphere around Y
click 1528 782
pause 0.5
click 1317 917
pause 3
shot proj-sphere
key ctrl+z
pause 1
# Auto, for comparison
click 1302 782
pause 0.5
click 1317 885
pause 4
shot proj-auto
