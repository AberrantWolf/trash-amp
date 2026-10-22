# Release gates

The unchecked items remain requirements for broader/stable releases. The
macOS alpha is limited to the scope and known issues in
[its release notes](releases/0.1.0-alpha.1.md); preparing an alpha does not
complete these gates. Build and draft-release steps are in
[the alpha release guide](alpha-release.md).

- [x] Folder libraries retain separate cached catalogs, with a saved-folder selector and explicit refresh. Existing single-library caches migrate automatically; cached folders can be reopened offline.
- [ ] Validate Slint on an iPhone: native permissions, text input, scaled text, VoiceOver, safe areas, navigation, suspend/resume, background audio, and native UI embedding/fallback.
- [ ] Measure UI frame pacing, CPU/GPU and idle/hidden-window energy in optimized builds with 2,000 albums; test marquee, scrolling, glow and a real FFT visualizer together.
- [ ] Provide accessible keyboard controls and OS reduced-motion/text-scale integration across the custom UI.
- [ ] Run the full real redumper/BIN sample comparison. The initial all-audio raw reader and explicit invalid-sample policy are implemented and synthetically tested; Q/index reconstruction, raw HTOA discovery and mixed-mode/multisession handling remain incomplete.
- [ ] Validate real single/multi-BIN, pregaps, HTOA, mixed-mode data exclusion, CD-TEXT and exact boundaries. Synthetic playback tests cover gap insertion, stored gaps, hidden first-track ranges and exclusion of data pregaps. Add player HTOA access and de-emphasis behavior.
- [ ] Validate gapless MP3/FLAC/Vorbis/CDDA through output capture. Resolve AAC/M4A trim/delay and Opus decoder gaps; don't advertise unsupported codecs.
- [ ] Test network stalls, disappearance/reconnection, rapid seeking, queue edits and device loss.
- [ ] Incremental catalog, progressive scans, bounded artwork caching, search and multi-disc grouping.
- [ ] Broaden MusicBrainz identification beyond complete audio TOCs/embedded release IDs/conservative name matching. Add manual ambiguity resolution and correction UI, Cover Art Archive and optional GnuDB. Keyless background names, response caching and match provenance are implemented.
- [ ] Platform media controls and lifecycle independent of UI; native device music libraries without mandatory app-folder imports.
- [x] Publish junk-libs playback changes and pin the player dependencies to `v0.1.0` (locked Git revision `a0abcdae1bafec8793ce77111f58aa9ab5675c4e`).
- [x] Replace Collector sibling path dependencies (including server test dependencies) with `v0.1.0-alpha.1`, locked to Git revision `eff206e3171ee028083bf35e98a8a27453069cad`. Builds no longer require sibling repositories.
- [ ] Platform packaging, dependency notices and signing. Validate macOS first, then Linux and Windows before claiming support.

Later: audiophile output modes, device selection/rate switching, effects, DLNA access, native mobile UI if needed. Playlists and equalizers are not part of the initial release scope.

Collector implementation and local evidence are recorded in
[Collector connection](collector-connection.md). Complete the remaining native
adversarial acceptance matrix and output-capture fidelity checks before declaring
that integration release-qualified. The existing local-library gates remain.
