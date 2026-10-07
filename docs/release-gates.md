# Release gates

These are required follow-up work, not completed features.

- [ ] **Multiple libraries: do not release while opening one library overwrites another library's cache.** Keep the current development shortcut visible until replaced.
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
- [ ] Publish junk-libs playback changes and replace the temporary sibling path dependency with a pinned Git revision.
- [ ] Platform packaging, dependency notices and signing. Validate macOS first, then Linux and Windows before claiming support.

Later: audiophile output modes, device selection/rate switching, effects, DLNA access, native mobile UI if needed. Playlists and equalizers are not part of the initial release scope.

Collector implementation and local evidence are recorded in
[Collector connection](collector-connection.md). Complete the remaining native
adversarial acceptance matrix and output-capture fidelity checks before declaring
that integration release-qualified. The existing local-library gates remain.
