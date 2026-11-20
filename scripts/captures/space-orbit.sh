# Holding Space (what a Huion pen button can send) turns a left drag into an orbit, and paints nothing.
# Stills: space-before, space-after.
click 774 640        # Sphere
pause 1.5
click 839 278        # Got it, on the first tip
drag 700 450 900 450 25
pause 0.5
move 1300 950
shot space-before
xdotool windowfocus --sync "$WIN"; xdotool keydown space; sleep 0.3
drag 700 600 900 600 25
xdotool keyup space; sleep 0.3
move 1300 950
pause 0.5
shot space-after
