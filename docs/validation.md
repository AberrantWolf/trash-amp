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
