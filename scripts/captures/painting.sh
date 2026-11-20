# Painting on the built-in cube: strokes across face seams, a fill, a mask stroke, then undo, undo, redo.
# Records target/captures/painting.mp4 and ends on a still, final.png.
click 842 641
pause 1
record painting
pause 0.5
move 700 500; xdotool mousedown 3
for i in $(seq 1 30); do xdotool mousemove $((700 + i*5)) 500; sleep 0.03; done
xdotool mouseup 3; sleep 0.5
drag 540 520 1060 400 60
pause 0.4
drag 800 260 760 780 60
pause 0.4
key g
key x
click 650 520
pause 0.6
key x
pause 0.8
key m
drag 560 640 1000 640 50
pause 0.8
key s
click 800 500
pause 1.2
key ctrl+z
pause 0.6
key ctrl+z
pause 0.6
key ctrl+shift+z
pause 1
move 800 840
pause 0.5
shot final
stop_record
