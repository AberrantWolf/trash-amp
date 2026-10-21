# Collector game-disc CD audio — 2026-10-21

The Collector adapter consumes game audio through the same listening, media,
playlist and cache contracts as music. Audio playlists explicitly use music_tracks;
Games collection inventory lists keep their default item behavior.

Isolated actual TLS tests cover mixed-mode discovery with track 1 excluded,
track 2 FLAC streaming and exact frame count, seeking, explicit download, server
shutdown/credential removal and offline reopen. Game artwork and title remain
available from the offline snapshot. Existing two-server identity/list and purchased
FLAC/MP3 tests also pass. The test uses generated media, not copyrighted game data.
The real output-device callback test remains opt-in; this adapter delivery does
not claim new physical-device acceptance.

Collector's independent owned CUE/BIN and browser qualification is recorded in its
2026-10-21 game-disc audio acceptance. CHD, mixed-mode raw redumper and proprietary
game sound formats remain outside that server delivery. Collector is pinned to `59faf341a0141c0424231d1700cfc16a006146a7`; all shared
libraries use `993afc932c6b2acb5cf02c95d3cb8a387d9a70a5`. Against those published
Git revisions, 32 affected audio/library/adapter tests pass with two existing
opt-in tests ignored. The Slint desktop checks, affected all-target strict Clippy
and workspace formatting pass. One fixed-array iteration repair keeps existing
audio sample handling unchanged under Rust 1.99 Clippy. A pre-existing dependency
future-incompatibility note for block 0.1.6 remains; it is not a current build error.

This feature branch was prepared in an isolated checkout. The operator's staged
native alpha packaging changes and working checkout were preserved unchanged.
Game audio is source-ready for a future native release; no macOS package was
published or signed as part of this milestone.
