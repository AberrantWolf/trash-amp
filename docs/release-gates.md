# macOS beta release checklist

Target: **0.1.0-beta.3**, Apple Silicon (arm64), macOS 15 or newer.
This is a preparation checklist, not a declaration that the beta is qualified.
Builds and automatic prerelease publication follow [the release guide](macos-release.md).
Record validation results, hardware, macOS version and the exact tested commit in
[validation.md](validation.md); qualify the extracted distribution package.

## Completed groundwork

- [x] Folder libraries retain separate saved catalogs, explicit refresh and offline browsing. Legacy single-library caches migrate without losing preferences or metadata; failed scans preserve the catalog.
- [x] Album/artist browsing, search, playback queue, keyboard transport and macOS media controls are implemented with automated coverage. Manual accessibility qualification remains below.
- [x] Conservative MusicBrainz enrichment, persistent response caching and provenance are implemented; local tags win and source music remains read-only.
- [x] Collector browsing, lists, authenticated streaming, integrity-checked downloads and offline retention have automated real-server coverage. Native FLAC/MP3 streaming/download/offline GUI acceptance is recorded in [Collector connection](collector-connection.md).
- [x] Collector artwork has a separate bounded 128 MiB cache; filesystem cover-art support remains later work.
- [x] Builds use hosted Git sources without sibling checkouts: junk-libs `2c8eaf8fd5a7f3e5f4873d3463b8008039e9af98`, Collector client/server test dependencies `c5587138c90f5d460b371cb9e489c8eb28aedb7a`. Cargo.lock pins dependency resolution.
- [x] Apple Silicon ZIP packaging includes release notes, application/dependency/native license notices and SHA-256 checksums. Package verification checks architecture, metadata, signature and dynamic dependencies.
- [x] Release automation accepts alpha and beta tags, verifies tag/version agreement, and publishes downloadable prereleases after validation, signing, notarization and package verification. Rerunning a published tag leaves its notes and assets unchanged.
- [x] Developer ID/Hardened Runtime signing, timestamping, notarization, stapling and extracted-package Gatekeeper checks are implemented. Tags require this path; local/PR/main development packages can remain ad-hoc signed. Generic Apple Developer team credentials use temporary runner-keychain cleanup and retained notarization diagnostics.
- [x] All six required repository secret names are configured. The signed Actions candidate imported the certificate using its password and confirmed that the identity matches the configured notarization team.
- [x] Output recovery pauses on stream failure, stalled callbacks or a default-device/configuration change, preserves the audible queue position and reopens the default output off the UI thread. Automated worker/callback tests cover recovery, rate changes, failed opens and offline transport; physical-device qualification remains below.

## Beta acceptance checklist

Published prereleases support testing on other machines. These checks track
remaining qualification; publication does not mark them complete or establish
support beyond the evidence recorded below.

- [x] Pass formatting, strict all-target Clippy, workspace tests, release-tooling tests, packaging and extracted-package verification in CI. [Signed candidate run 39885698740](https://github.com/AberrantWolf/trash-amp/actions/runs/39885698740) passed on `554e77a`: 53 Rust tests (two opt-in tests skipped) and 15 release-tooling tests. The release tag must repeat these checks for its final commit.
- [x] Publish beta.2 after final tagged checks. [Tagged run 40148089210](https://github.com/AberrantWolf/trash-amp/actions/runs/40148089210) passed on `0aa96d2`: 67 Rust tests, 28 release-tooling tests, signing/notarization/stapling, extracted-package verification and automatic publication. The independently downloaded ZIP passed local signed-package and Gatekeeper checks; see [validation](validation.md#published-beta2--2026-10-25).
- [x] Run a signed manual candidate and confirm Apple's acceptance, stapling and extracted-package Gatekeeper verification. Apple accepted submission `a835f3f1-ce2b-4043-bdb7-d6f8a1c95b7a` with no issues; both the hosted runner and this Mac verified the downloaded package as Notarized Developer ID. See [the signed validation record](validation.md#completed-signed-candidate--2026-10-23).
- [ ] Verify first launch of the downloaded, Developer ID-signed and notarized package on another Mac. Local signing validation alone does not complete this gate.
- [ ] Verify clean installation and upgrade with existing preferences, saved folders, Collector credentials, lists, downloads and artwork. Retest Keychain behavior using the final signing identity, including connection removal and offline reopen.
- [ ] Qualify output-device loss/change, USB/Bluetooth reconnect, sleep/wake and playback after resume in the native app. Verify that recovery preserves position, leaves playback paused until Play, keeps the UI responsive and honors explicit Play/Pause requests while disconnected. Include a replacement output with a different sample rate and repeated disconnects.
- [ ] Exercise rapid seek/next/previous, queue edits, source switching and unavailable files during playback. Verify accurate state, cancellation and actionable errors without hangs.
- [ ] Exercise stalled/disconnected/reconnected network shares and Collector routes in the native app. Cover denied/revoked access, changed media, two-account isolation, interrupted downloads, cancel/retry and restart recovery. Automated fixtures supplement the native failure matrix; see [Collector connection](collector-connection.md).
- [ ] Validate playback of each advertised format (WAV/FLAC/MP3/Vorbis/CDDA) through the actual output path. Use output capture to qualify gapless claims; retain explicit limits for AAC/M4A, Opus, multichannel and bit-perfect output.
- [ ] Run real redumper/BIN and CUE boundary comparisons before qualifying those paths, or explicitly label them experimental in the final release notes. Keep raw mixed-mode/multisession, HTOA controls and unapplied de-emphasis limits visible.
- [ ] Check keyboard-only navigation, focus, VoiceOver labels, text scaling and reduced motion throughout browsing, transport, settings and lists. Verify media keys and Now Playing from the packaged app.
- [ ] Measure optimized large-library scrolling/search, UI responsiveness, memory, idle/playing CPU and hidden-window energy with real libraries and the 2,000-album demo. Record hardware and window dimensions; address hangs or unbounded growth.
- [ ] Review [beta release notes](releases/0.1.0-beta.3.md) against native acceptance evidence, document remaining known issues and Collector compatibility, and verify the issue-reporting instructions.
- [ ] Download the published ZIP, verify its checksum, and complete the separate-Mac smoke test: Finder launch, version/Slint attribution, folders/restart, local audio, Collector enrollment/streaming/download/offline playback and media keys.

## Later work

These features do not block the scoped macOS beta. Revisit qualification before
expanding advertised support.

- Incremental/progressive scans, filesystem artwork caching and multi-disc grouping.
- Manual metadata correction/ambiguity resolution, broader identification, Cover Art Archive and optional GnuDB.
- AAC/M4A gapless and Opus support; multichannel mapping; complete disc handling including HTOA access, de-emphasis, raw mixed-mode/multisession and Q/index reconstruction.
- Native output selection/rate switching, audiophile output modes, effects and DLNA. Playlists and equalizers remain outside the initial scope.
- Marquee and FFT visualization, with combined animation performance/energy measurements when implemented.
- Linux/Windows packaging, signing and native acceptance before claiming support.
- iPhone validation: permissions, text input/scaling, VoiceOver, safe areas, navigation, suspend/resume, background audio and native UI embedding/fallback. Native device music libraries and lifecycle work accompany mobile scope.
