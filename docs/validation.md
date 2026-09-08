# Prototype validation

Automated tests cover exact decoded WAV samples, seek offset, resampling length including short/partial chunks, SQLite settings/library replacement, cancellation, and unavailable roots.

Manual procedure:

1. Run an optimized build with `--demo`. Resize between 980×680, 390×740 and 640×280. Select albums, browse tracks, open settings and enable reduced motion. Repeat with text scaling.
2. Open a real single-album fixture and a mounted multi-album library. Test start, pause, resume, seek, next and previous. Verify no music-folder writes.
3. Use known contiguous WAV/CD tracks and loopback capture to validate that joining tracks neither inserts silence nor drops/repeats samples. Listening alone is insufficient.
4. Repeat seeking while paused and under network delay; test a disconnected share and an unavailable audio device.
5. Compare CPU, memory, frame timing and energy with idle/playing, visible/hidden, animation enabled/disabled, using release builds. Report measurements with hardware and window dimensions.

The generated demo exercises layout only. The level meter is real peak amplitude, not an FFT spectrum. The prototype does not yet satisfy the full performance/mobile validation gate.

## Results from the first macOS development build

- `cargo test --workspace`: 7 tests passed, including exact WAV decoding/seek, resampler length, continuous two-track worker output, CUE track boundaries and within-sector seeking, SQLite roundtrip/replacement, cancellation and unavailable paths.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all --check`: passed.
- Native Skia renders inspected at 980×680, 390×740, 640×280 and 800×700 with 1.5× text. Fixed the narrow-layout switch and header sizing found during inspection. Screenshots in `screenshots/` use generated data.
- `cargo run -p amp-audio --example output_probe`: the default macOS device's output clock advanced to 1.013 seconds with zero callback underruns in the short silent probe. This checks device plumbing, not output fidelity or long-running reliability.

Not yet measured: optimized animation frame time/energy, real network-drive performance, actual compressed-album gaplessness, or mobile behavior. No claim of a completed GUI performance comparison.

Renderer snapshot command (development utility):

```sh
cargo run -p trash-amp -- --demo --select-first --size 390x740 --snapshot /tmp/trash-amp.png
```

## Initial usability fixes

- 12 workspace tests pass; formatting and Clippy with warnings denied pass.
- The transport slider regression sends real pointer events through Slint's testing backend, then verifies that subsequent clock updates and restart positions still reach the thumb after repeated scrubbing.
- Tagged WAV fixtures verify compilation merging across artist-first paths and within one folder, preservation of conflicting copies/explicit album artists, and separation of untagged folders.
- Native renders inspected for matching SVG transport icons and the preferences footer at 980×760 and 640×360. The attribution stays fixed-size at the panel bottom when it fits, and below the scroll viewport when it does not.
- Reopen the library to refresh an existing cached snapshot with the new grouping rules.

## Disc playback implementation

- Added synthetic raw-versus-BIN comparison through the player's actual reader/resampler path, covering both tracks, repeated starts, frame-level seeks, EOF and C2 rejection.
- Added filesystem-provider tests for raw-only discovery, avoiding duplicate raw/CUE albums, raw/BIN preference and explicit invalid-raw errors. Reopen cached libraries after updating.
- junk-libs has separate tests for stereo ordering, exact seeks, signed-range bounds, source truncation, quality states, TOC validation, stored/synthetic CUE gaps, hidden first-track ranges, and stopping before data pregaps.
- Real redumper full-disc comparison is registered as an ignored test requiring the documented Octopath fixture prefix. It has not yet been run; no claim of verified real-dump or captured-device gaplessness is made.
- The sibling junk-libs workspace's existing lockfile requires unrelated dependency resolution, so package tests/clippy were run from a temporary mirror with an independently resolved lockfile. Its tracked lockfile and unrelated crates were left unchanged. Player tests use the actual sibling source.

## Keyless metadata

- The actual Rust HTTP client successfully queried MusicBrainz's documented `I5l9cCSFccLKFEKS.7wqSZAorPU-` disc and matched Nevermind/Nirvana with all 12 track names. The opt-in `live_documented_disc_lookup_and_names` test is ignored during normal offline test runs.
- Deterministic tests cover the published disc-ID vector, CUE initial synthetic pregaps, mixed-mode exclusion, edition consensus/ambiguity, wrong media/track counts, duplicate positions, strict search duration/title matching, preservation of existing names and playback access, positive/negative/stale caching, offline reuse, in-flight cancellation, failures and unchanged media-file contents.
- Desktop enrichment runs after the local scan and on cached-library startup. New scan generations reject stale updates; individual rows and current queue labels update without replacing playback positions.
- Builds use the user's normal Cargo home; no temporary Cargo-home override is used for this change.
