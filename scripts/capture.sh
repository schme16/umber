#!/bin/bash
# Screenshots and videos of Umber on a hidden X display, never on the desktop someone is using.
#
#   scripts/capture.sh <steps.sh> [umber args...]
#
# Starts Xvfb (1600x1000, or SIZE=WxH), runs the debug build on it, then sources <steps.sh> with these helpers:
#   move X Y        put the pointer at window coordinates (the window sits at 0,0)
#   click X Y       move there and click (left button)
#   drag X1 Y1 X2 Y2 [steps]   press, move in steps, release
#   key KEYS        xdotool key syntax, e.g. key ctrl+o
#   pause SECONDS   let frames settle
#   shot NAME       PNG of the whole screen -> $OUT/NAME.png
#   record NAME     start an MP4 of the screen -> $OUT/NAME.mp4
#   stop_record     finish it
#
# Vulkan draws on the real card under Xvfb (only presenting goes through X), so this is the
# GPU path, not llvmpipe: the log line "GPU: ..." in $OUT/umber.log says which adapter ran.
# Output goes to $OUT (default target/captures). BIN=target/release/umber captures the release
# build, for anything that has to keep up with the pointer.
set -euo pipefail

SIZE=${SIZE:-1600x1000}
if [ -z "${UMBER_IN_XVFB:-}" ]; then
    export UMBER_IN_XVFB=1 SIZE
    exec xvfb-run -a -s "-screen 0 ${SIZE}x24" "$0" "$@"
fi

STEPS=$(realpath "$1"); shift
cd "$(dirname "$0")/.."
OUT=${OUT:-target/captures}
mkdir -p "$OUT"

# A fresh preferences folder unless one is given, so first-run tips show as they would for
# someone new, and the captures never touch the real one.
export UMBER_CONFIG_DIR=${UMBER_CONFIG_DIR:-$(mktemp -d)}
# Likewise recovery copies, so a capture never leaves "unsaved work" on the real welcome card.
export UMBER_RECOVERY_DIR=${UMBER_RECOVERY_DIR:-$(mktemp -d)}

WGPU_BACKEND=vulkan "${BIN:-./target/debug/umber}" "$@" > "$OUT/umber.log" 2>&1 &
APP=$!
REC=""
cleanup() {
    [ -n "$REC" ] && kill -INT "$REC" 2>/dev/null && wait "$REC" 2>/dev/null || true
    kill "$APP" 2>/dev/null || true
}
trap cleanup EXIT

WIN=""
for _ in $(seq 1 80); do
    WIN=$(xdotool search --name 'Umber$' 2>/dev/null | head -1 || true)
    [ -n "$WIN" ] && break
    sleep 0.25
done
[ -n "$WIN" ] || { echo "Umber window never appeared; see $OUT/umber.log" >&2; exit 1; }
xdotool windowmove "$WIN" 0 0
xdotool windowsize "$WIN" "${SIZE%x*}" "${SIZE#*x}"
sleep 2

move() { xdotool mousemove "$1" "$2"; sleep 0.15; }
click() { move "$1" "$2"; xdotool mousedown 1; sleep 0.05; xdotool mouseup 1; sleep 0.4; }
drag() {
    local n=${5:-20}
    move "$1" "$2"; xdotool mousedown 1
    for i in $(seq 1 "$n"); do
        xdotool mousemove $(( $1 + ($3 - $1) * i / n )) $(( $2 + ($4 - $2) * i / n )); sleep 0.02
    done
    xdotool mouseup 1; sleep 0.3
}
# No window manager runs under Xvfb, so give the window keyboard focus first.
key() { xdotool windowfocus --sync "$WIN" 2>/dev/null || true; xdotool key "$@"; sleep 0.3; }
pause() { sleep "$1"; }
shot() { import -window root "$OUT/$1.png"; echo "$OUT/$1.png"; }
record() {
    ffmpeg -loglevel error -y -f x11grab -framerate 30 -video_size "$SIZE" -i "$DISPLAY" \
        -c:v libx264 -pix_fmt yuv420p -crf 22 -movflags +faststart "$OUT/$1.mp4" &
    REC=$!; sleep 0.5
}
stop_record() { kill -INT "$REC"; wait "$REC" 2>/dev/null || true; REC=""; }

# shellcheck source=/dev/null
source "$STEPS"
grep -m1 "GPU:" "$OUT/umber.log" || true
