# The command palette, the shortcut sheet and the first tip, on the sphere.
# Stills: help-tip, help-palette, help-search, help-reason, help-sheet, help-uv-tip.
click 774 640        # Sphere on the welcome card
pause 1.5
shot help-tip
key ctrl+k
pause 0.6
shot help-palette
xdotool type --delay 60 "bigger brush"
pause 0.6
shot help-search
key ctrl+a
xdotool type --delay 60 "repack"
pause 0.6
key ctrl+a
xdotool type --delay 60 "redo"
pause 0.6
shot help-reason
key Escape
pause 0.4
key question
pause 0.6
shot help-sheet
key Escape
pause 0.4
