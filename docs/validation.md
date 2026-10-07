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

## Library search and artist browsing

- 42 workspace tests pass; the existing live MusicBrainz smoke test remains ignored. Added coverage for accent/cross-field matching, whole artist credits and name boundaries, grouped appearances, old preference compatibility, ID-based result actions after sorting, Back and clear-search behavior, metadata refresh, stale worker replies, and explicit playback queue composition.
- Slint regression tests exercise text editing versus playback keys, Cmd/Ctrl+F after returning to the phone library, delayed pointer navigation, detail-pane width, and full 100,000-track results. The large-result test checks that fewer than 100 row delegates exist both initially and after scrolling.
- Optimized synthetic measurement on this development Mac: 10,000 albums / 100,000 tracks indexed in approximately 204 ms; a multi-word search took approximately 11 ms. These are a single local benchmark, not a cross-device latency guarantee.
- Actual native Skia snapshots inspected at 980×760, 390×740, 800×700 with 1.5× text, and 640×280. Inspected grouped search, album-artist browsing, and a page with many one-track appearances. Corrected artist-label alignment, a Back-button constraint that narrowed the detail pane, and unnecessarily repeated album titles in appearance rows.
- GUI QA uses generated tracks and a temporary SQLite directory. New playback behavior is checked through sequence composition without requiring an audio output device; this does not claim new physical-device or mobile-platform validation.

Reproduce search and appearance renders (the demo has no playable audio):

```sh
TRASH_AMP_DATA_DIR=/tmp/trash-search-qa cargo run -p trash-amp -- --demo --search late --size 980x760 --snapshot /tmp/trash-search.png
TRASH_AMP_DATA_DIR=/tmp/trash-search-qa cargo run -p trash-amp -- --demo --demo-appearances --browse-artists --artist "The Late Hours" --detail-scroll 530 --size 390x740 --snapshot /tmp/trash-artist.png
cargo test -p amp-core --release ten_thousand -- --nocapture
```

The QA switches `--search`, `--browse-artists`, `--artist`, `--demo-appearances`, and `--detail-scroll` affect the current demo session; they do not save browser preferences.

## Collector integration, 2026-10-07

See [implementation and coverage](collector-connection.md). Workspace tests and
Clippy with warnings denied passed. The actual macOS app restored its server
selection and artwork after restart, retained album/list selection after refresh,
and retained cached lists while disabling mutations after server shutdown.
Native FLAC playback and explicit download completed. The audible synthetic ramp
is not music fidelity evidence; output capture remains required. Collector
workspace/architecture/Clippy/web gates and browser smoke passed, and phono-junk's
GUI consumer compiled without source edits.

## macOS icon and bundle, 2026-10-07

The app embeds its generated 1024px PNG window icon and bundles a multi-resolution
ICNS through `scripts/bundle-macos.sh`. The debug bundle was built and opened as
“Trash Amp” using an isolated temporary profile; no audio was played. `plutil`
validated its Info.plist and `codesign --verify --deep --strict` accepted the
ad-hoc signature. Formatting, workspace tests and Clippy with warnings denied
passed again after adding the icon. Distribution signing/notarization remain
release gates.
