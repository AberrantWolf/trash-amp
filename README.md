# Trash Amp

An album-focused Rust music player. The first development target is macOS, with a Slint UI over independent library and audio crates.

## Run

```sh
cargo run -p trash-amp --release
cargo run -p trash-amp --release -- --library /Volumes/Music
cargo run -p trash-amp --release -- --demo
```

`--demo` loads 2,000 generated albums for layout testing. They contain no audio.
Use `TRASH_AMP_DATA_DIR=/some/local/directory` to isolate the SQLite cache during development. The default is the platform's application data directory. Never put this cache on the music share.

The initial build downloads Slint/Skia dependencies and can take several minutes. A Rust toolchain and macOS Xcode command-line tools are required. The lockfile pins the development dependency set. Other targets have not yet been validated.

## Current development slice

- Size-adaptive album/track browsing, compact transport, a playback level meter, and reduced-motion preference.
- Filesystem libraries, including mounted network shares and single-album folders.
- Read-only tag inspection, separate album copies, CUE/BIN track discovery, CD-TEXT album fields.
- Local SQLite library snapshot and preferences. Failed scans preserve the previous library. A new scan cancels the previous scan between file operations.
- Streaming Symphonia decoding and seek, CUE/BIN PCM through junk-libs, CPAL output, and Rubato resampling when needed.
- Transient album playback order and up-next controls.

This is a prototype, not a release candidate. The library scan currently refreshes a complete snapshot; incremental per-file caching, progressive results, artwork, online metadata, text marquee, FFT visualization, and system media controls are not implemented yet. The automatic-metadata preference is saved for the future provider integration; it currently sends no requests.

Supported decoding is determined by the pinned Symphonia version. WAV/FLAC/MP3/Vorbis and CDDA are the first validation targets. AAC/M4A gapless playback and Opus require additional work before they can be advertised as supported. Stereo and mono are supported; multichannel needs an explicit channel mapper. Do not interpret successful playback as verified bit-perfect output.

Redumper `.scram/.state/.subcode` packages are discovered and structurally validated, but raw playback is **not implemented**. The default preference selects valid raw packages and reports that missing decoder rather than silently playing a different representation. To test equivalent BIN/CUE files, disable “Prefer redumper raw dumps” and reopen the folder.

## Verify

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

See [architecture](docs/architecture.md), [release gates](docs/release-gates.md), and [prototype validation](docs/validation.md).

## junk-libs development

The normal dependency is pinned to a Git revision. To use your sibling checkout without changing the portable manifest:

```sh
cargo run --config 'patch."https://github.com/AberrantWolf/junk-libs".junk-libs-disc.path="../junk-libs/junk-libs-disc"' --config 'patch."https://github.com/AberrantWolf/junk-libs".junk-libs-core.path="../junk-libs/junk-libs-core"' -p trash-amp
```

## License

Application source: MIT. Slint is used under its Royalty-free Desktop, Mobile, and Web Applications License with the AboutSlint attribution in Settings. Other dependencies retain their respective licenses; distributed packages must include their notices. See https://slint.dev/terms-and-conditions.
