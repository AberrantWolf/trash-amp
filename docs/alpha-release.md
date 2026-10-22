# Preparing the macOS alpha

The first alpha is `v0.1.0-alpha.1`, Apple Silicon only, with macOS 15 as its
minimum deployment target. Rust is pinned by `rust-toolchain.toml` and Cargo
dependencies by the committed lockfile. All external workspace crates come
from pinned revisions in hosted Git repositories; sibling repositories are not needed.

## Local package

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
./scripts/package-macos.sh
./scripts/verify-macos-package.sh target/dist/*.zip
```

Python 3.12 or newer, Xcode command-line tools, and network access are required.
The real Collector server test needs `flac` on PATH (`brew install flac`). Notice
generation downloads cargo-about 0.9.2 and verifies its pinned SHA-256 checksum.
Changing Slint's license or the Skia version requires updating the bundled
license texts; packaging stops until they match.

The ZIP and SHA-256 file appear in `target/dist`. It includes the ad-hoc signed
app bundle and release notes. Distribution signing and notarization are not
configured, so the alpha notes explain macOS's first-launch approval.

## GitHub build and draft release

`.github/workflows/macos-alpha.yml` validates and packages pull requests, main
pushes and manual runs. These runs upload a downloadable Actions artifact.
Pushing an alpha tag also creates a **draft prerelease** only after validation,
packaging and checksum verification succeed. Re-running the tag workflow may
update a draft, but refuses to replace an already published release.

Once the reviewed changes are on main and its Actions run passes:

```sh
git tag -a v0.1.0-alpha.1 -m 'Trash Amp 0.1.0 alpha 1'
git push origin v0.1.0-alpha.1
```

Download and test the draft's ZIP on a separate Mac before publishing. Confirm
Finder launch, settings/version and Slint attribution, folder switching and
restart, local audio, Collector enrollment/streaming/downloads, and media keys.
Publish the draft manually after that review. Broader requirements remain in
`docs/release-gates.md`; the alpha does not claim those are complete.
