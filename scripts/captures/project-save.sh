# Project files: paint the cube, see it marked unsaved, save with Ctrl+S, change it again,
# then Ctrl+Q shows "Save changes?". Run with UMBER_SAVE_PATH and UMBER_RECOVERY_DIR set.
click 842 641
pause 1
drag 540 520 1060 400 60
pause 0.4
drag 800 260 760 780 60
pause 0.8
move 800 900
pause 0.5
shot unsaved
key ctrl+s
pause 2.5
shot saved
drag 560 640 1000 640 50
pause 0.8
key ctrl+q
pause 1
shot close-prompt
