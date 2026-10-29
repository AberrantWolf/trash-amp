# Digital album matching

Set an AcoustID application key in Settings. Select a local folder album and choose
**Find matching albums**. The explicit action reads the selected FLAC/MP3 files in
background work and sends fingerprints/durations to AcoustID, then queries linked
MusicBrainz releases. Audio files and paths stay local. Closing the review does not
cancel work; reopen it from the album, or use Cancel matching.

Review artwork, coverage and per-file assignments. **Use this album and track
mapping** is enabled only for complete unambiguous mappings without conflicting
known durations. Existing source/manual names and original files are preserved.
A changed source or changed catalog generation invalidates application. Accepted
metadata survives offline rescans when the exact source files still match.

For Collector albums, **Review matching in Collector** opens that server's existing
metadata workflow. Configure the server's MusicBrainz contact and AcoustID key
there; its curator permissions and listener exposure still apply.

The shared crate is `junk-libs-music`; fingerprint calculation pins corrected
chromaprint-next source, and MusicBrainz decoding uses musicbrainz_rs 0.14 models
with consumer-owned transport and safe URL encoding. Published chromaprint-next
0.1.0 has a stereo streaming bug and is not selected. Native Chromaprint 1.6.2
provides independent algorithm vectors; no native runtime dependency is added.

The bounded search handles at most 256 files, 32 recording IDs and 32 full release
candidates; it reads at most the first 100 links per recording and previews eight
covers. Partial/ambiguous results remain suggestions. No exhaustive search,
additional codec support or catalog verification is claimed. Live AcoustID
recognition needs an application key and has not been qualified with real music.
See [validation](validation.md) for offline workflow and UI acceptance.

The fingerprint implementation includes LGPL-2.1-or-later code. Runtime notice
generation includes that license. A distribution must retain the complete matching
application source, Cargo.lock, pinned dependency sources and build instructions
so recipients can rebuild with a modified fingerprint library. Source distributions
are available from the application's tagged repository and exact Git revisions
in Cargo.lock; build with `cargo build --locked -p trash-amp` using the documented
Rust toolchain. Verify corresponding-source availability as part of release
packaging; no application binary is distributed by this development delivery.
