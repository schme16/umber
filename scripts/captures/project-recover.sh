# Crash recovery: paint the cube, wait for the recovery copy, kill Umber outright, start it
# again and recover the work from the welcome card. Run with UMBER_RECOVERY_DIR set.
click 842 641
pause 1
drag 540 520 1060 400 60
drag 800 260 760 780 60
pause 4
kill -9 "$APP"; sleep 0.5
WGPU_BACKEND=vulkan ./target/debug/umber >> "$OUT/umber.log" 2>&1 &
APP=$!
sleep 3
WIN=$(xdotool search --name 'Umber$' | head -1); xdotool windowmove "$WIN" 0 0; xdotool windowsize "$WIN" 1600 1000
pause 2
shot recover-offer
