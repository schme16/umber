# App icon

A cube with a brass brush stroke running over its corner, the way a stroke wraps round a model
in Umber, on a transparent background so it sits on any panel or dock. Chosen from ten options on 2026-11-20.

- `umber.svg`: the icon, for 48 px and up.
- `umber-small.svg`: the cut for 16 to 32 px, with a heavier, simpler stroke.
- `umber-256.png`: rendered from `umber.svg`; compiled into the app as the window icon.

Re-render the PNG after changing the SVG: `rsvg-convert -w 256 -h 256 umber.svg -o umber-256.png`.
