# Trash Amp application icon

`trash-amp.png` is the 1024px master. `TrashAmp.icns` contains standard 16–512px
macOS icon sizes and their Retina variants. Run `scripts/build-icon.sh` on macOS
to regenerate the ICNS. The Slint window embeds the PNG; the app bundle declares
the ICNS for Finder and the Dock.

Generated with the built-in imagegen tool, then resized for packaging with sips.
Prompt: a polished macOS icon showing a squat galvanized metal trash can
reimagined as a hi-fi speaker, dark circular driver with a warm orange center,
slightly tilted lid, charcoal rounded-square tile, transparent outer margin,
soft studio highlights, crisp silhouette; no lettering or musical notes.
