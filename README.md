# Trash Amp

An album-focused music player written in Rust, with a [Slint](https://slint.dev) UI.
It plays local folders (including network shares), CUE/BIN and redumper disc
images, and Collector servers, and fills in missing names from MusicBrainz.

macOS is the primary target. Beta builds for Apple Silicon (macOS 15+) are on the
[Releases](https://github.com/AberrantWolf/trash-amp/releases) page. Linux builds
and runs from source; iOS is an early development host.

## Building

All platforms need the Rust toolchain; `rust-toolchain.toml` selects the pinned
version automatically. The first build takes a few minutes while Slint and Skia
compile.

### macOS

Install the Xcode command-line tools, then build an app bundle:

```sh
./scripts/bundle-macos.sh --open          # release
./scripts/bundle-macos.sh --debug --open  # faster development build
```

Bundling also needs Python 3. Signing and release packaging are covered in the
[macOS release guide](docs/macos-release.md).

### Linux

Tested on Kubuntu (Wayland). Install the native development libraries:

```sh
sudo apt install libasound2-dev libdbus-1-dev libfontconfig-dev libfreetype-dev libxkbcommon-dev
# Only needed for `cargo test --workspace` (Collector server tests):
sudo apt install libssl-dev flac ffmpeg
```

Other distributions need the equivalent ALSA, D-Bus, fontconfig, FreeType and
xkbcommon development packages. The build also uses CMake and a C compiler.

Collector logins are saved through Secret Service (KWallet or GNOME Keyring),
which must be running and unlocked. Linux is not yet a release platform; media
keys and file dialogs are unverified there.

### iOS

See [iOS development](docs/ios-development.md).

## Running

```sh
cargo run -p trash-amp --release
cargo run -p trash-amp --release -- --library /path/to/music
cargo run -p trash-amp --release -- --demo   # 2,000 generated albums, no audio
```

The library cache and settings live in the platform's application data
directory. Set `TRASH_AMP_DATA_DIR=/some/local/dir` to use a separate one during
development. Keep the cache on a local disk, not the music share.

## Development

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Further documentation:

- [Architecture](docs/architecture.md)
- [Playback formats and metadata privacy](docs/playback-and-metadata.md)
- [Release gates](docs/release-gates.md) and [release notes](docs/releases/)
- [Collector connection](docs/collector-connection.md)
- [Digital album matching](docs/digital-album-matching.md)
- [Device music library](docs/device-music-library.md)

## License

MIT. Slint is used under its
[Royalty-free Desktop, Mobile, and Web Applications License](https://slint.dev/terms-and-conditions),
with the required attribution shown in Settings. Other dependencies keep their own
licenses, and distributed packages must include their notices.
