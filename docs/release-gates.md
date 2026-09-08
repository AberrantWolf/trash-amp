# Release gates

These are required follow-up work, not completed features.

- [ ] **Multiple libraries: do not release while opening one library overwrites another library's cache.** Keep the current development shortcut visible until replaced.
- [ ] Validate Slint on an iPhone: native permissions, text input, scaled text, VoiceOver, safe areas, navigation, suspend/resume, background audio, and native UI embedding/fallback.
- [ ] Measure UI frame pacing, CPU/GPU and idle/hidden-window energy in optimized builds with 2,000 albums; test marquee, scrolling, glow and a real FFT visualizer together.
- [ ] Provide accessible keyboard controls and OS reduced-motion/text-scale integration across the custom UI.
- [ ] Implement raw redumper reading with sample-exact alignment and invalid-sample policy; default priority remains raw. Compare fixtures against equivalent BIN/CUE without double offset correction.
- [ ] Test single/multi-BIN, pregaps, HTOA, mixed-mode data exclusion, CD-TEXT and exact boundaries.
- [ ] Validate gapless MP3/FLAC/Vorbis/CDDA through output capture. Resolve AAC/M4A trim/delay and Opus decoder gaps; don't advertise unsupported codecs.
- [ ] Test network stalls, disappearance/reconnection, rapid seeking, queue edits and device loss.
- [ ] Incremental catalog, progressive scans, bounded artwork caching, search and multi-disc grouping.
- [ ] MusicBrainz identification, Cover Art Archive, optional GnuDB, ambiguity/provenance and cache-only corrections.
- [ ] Platform media controls and lifecycle independent of UI; native device music libraries without mandatory app-folder imports.
- [ ] Platform packaging, dependency notices and signing. Validate macOS first, then Linux and Windows before claiming support.

Later: audiophile output modes, device selection/rate switching, effects, DLNA access, native mobile UI if needed. Playlists and equalizers are not part of the initial release scope.
