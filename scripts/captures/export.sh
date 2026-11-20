# Export to Godot on the built-in cube after a few strokes: the Export panel before and after.
# Run with UMBER_EXPORT_DIR set (a folder picker would open on the real desktop), ideally a
# folder inside a Godot project. Stills: painted, export-panel, exported.
click 842 641
pause 1
drag 540 520 1060 400 60
pause 0.4
key x
# A raised stripe: height and metal as well as colour.
click 587 130
click 653 130
drag 800 260 760 780 60
pause 0.6
shot painted
click 898 26
pause 0.8
shot export-panel
click 1423 623
pause 3
shot exported
