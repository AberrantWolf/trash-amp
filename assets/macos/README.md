# Trash Amp application icon

`trash-amp.png` is the 1024px master. `TrashAmp.icns` contains standard 16–512px
macOS icon sizes and their Retina variants. Run `scripts/build-icon.sh` on macOS
to regenerate the ICNS. The Slint window embeds the PNG; the app bundle declares
the ICNS for Finder and the Dock.

The current design is **Double Parked**: deliberately improbable headphones
with three oversized earcups, a thick ivory headband and cushions, and orange
gradient outer shells on a charcoal rounded-square tile. The left and lower-right
cups turn inward. The outer margin is transparent.

Generated and refined with the built-in imagegen tool, then resized to the
1024px master and packaged with `sips` and `iconutil`.

Design prompt: refine the three-cup Double Parked headphone icon into a dense,
bold silhouette with a substantially thicker headband and wider matching cups
extending inward to reduce the empty center. Preserve the left cup's inward
tilt and the stronger inward turn of the lower-right cup. Keep the cushions and
headband ivory; give all three outer shells a subtle amber-to-ember orange
gradient. Use simple flat shapes on a charcoal rounded-square tile, without
lettering, wires, musical notes, or extra details.

Final preparation prompt: remove only the off-white background outside the
rounded-square tile, replacing it with alpha transparency. Preserve the opaque
tile, three cups, colors, angles, spacing, and proportions; retain the square
canvas and narrow outer margin with an antialiased boundary.
