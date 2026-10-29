# Trash Amp

An album-focused Rust music player. The first development target is macOS, with a Slint UI over independent library and audio crates.

The current testing release is **0.1.0-beta.4**, for Apple Silicon Macs on macOS 15
or newer. Download it from [Releases](https://github.com/AberrantWolf/trash-amp/releases).
Beta acceptance testing is in progress.
See the [beta checklist](docs/release-gates.md),
[release notes](docs/releases/0.1.0-beta.4.md), and
[release guide](docs/macos-release.md) for qualification, packaging and the GitHub
automated prerelease workflow.

## macOS app bundle

```sh
./scripts/bundle-macos.sh --open
# Faster development build:
./scripts/bundle-macos.sh --debug --open
```

This creates `target/release/Trash Amp.app` (or `target/debug/Trash Amp.app`) with
its own Finder and Dock icon. Move the bundle into Applications if desired.
The bundle uses an ad-hoc development signature by default. Developer ID signing
and notarized release packaging are described in the [release guide](docs/macos-release.md).
Bundling requires Python 3 and the Xcode command-line tools in addition to Rust.

## iOS development

An initial iPhone/iPad host shares the Slint UI with desktop. It is a development
bootstrap; background playback and device qualification remain unfinished.
See [iOS development](docs/ios-development.md) for simulator builds, signing and
remaining work.

## Run

```sh
cargo run -p trash-amp --release
cargo run -p trash-amp --release -- --library /Volumes/Music
cargo run -p trash-amp --release -- --demo
```

`--demo` loads 2,000 generated albums for layout testing. They contain no audio.
Use `TRASH_AMP_DATA_DIR=/some/local/directory` to isolate the SQLite cache during development. The default is the platform's application data directory. Never put this cache on the music share.

Each successfully scanned folder keeps a separate saved catalog. Under **Local
folder**, use the saved folder selector to reopen one immediately, including
while its network share is disconnected. **Refresh folder** rescans for changes;
a failed scan keeps the saved catalog. Playback still requires the source files
to be available. The last selected folder is restored on startup when using the
local source. Existing single-library caches migrate automatically.

The initial build downloads Slint/Skia dependencies and can take several minutes. A Rust toolchain and macOS Xcode command-line tools are required. The lockfile pins the development dependency set. Other targets have not yet been validated.

## Drop files to play

On desktop, drop music files or folders anywhere on the player to make a temporary
playlist. Folders are searched recursively, filtering unsupported files as they
are encountered. Dropped items keep their supplied order; folder contents use
natural path order (2 before 10). Playlist files and disc-image packages are skipped.

The first discovered track replaces the current queue and starts playing while
more files are found and added to **Up next**. A nonmodal progress strip shows
counts and offers **Cancel**; playback controls and library browsing remain
available. Cancelling stops discovery and keeps tracks already added. Choosing
different music or clearing Up next also stops discovery, so late results cannot
refill or change the replacement queue. If no usable files are found, existing
playback is preserved. Missing or unreadable files are skipped. Files are played
directly, without copying them or adding them to a library; the playlist lasts
only for this session.

## Current development slice

- Size-adaptive album/track browsing, compact transport, a playback level meter, and reduced-motion preference.
- Compact icon actions with desktop hover tooltips, keyboard controls, and screen-reader names. iOS/Android omit hover tooltips. Home holds source/library selection, refresh, folder opening, Lists, and Settings; a Home icon returns there without a toolbar above the tracks.
- Filesystem libraries, including mounted network shares and single-album folders.
- Development builds add a read-only [Device Music Library](docs/device-music-library.md) source on Mac, iPhone and iPad for local, unprotected songs. Physical synced-library qualification remains pending.
- Read-only tag inspection, compilation merging across artist folders, separate conflicting album copies, CUE/BIN track discovery, CD-TEXT album fields.
- Local SQLite library snapshot and preferences. Failed scans preserve the previous library. A new scan cancels the previous scan between file operations.
- Streaming Symphonia decoding and seek, CUE/BIN and current redumper audio PCM through junk-libs, CPAL output, and Rubato resampling when needed.
- Keyless MusicBrainz metadata retrieval, a persistent local response cache, and background name updates.
- Transient album playback order and up-next controls, with Tracks / Up next tabs.
- A draggable desktop album divider; double-click to reset. Its width survives phone/desktop layout switches within the window.
- Albums / Artists browsing, persisted locally; artist entries retain complete album-artist credits and show album counts.
- Live library search across artists, albums, and tracks, with accent-insensitive multi-word matching, grouped previews, and full result lists.
- Artist pages with primary albums and compact album-grouped guest appearances; Back restores search and browsing positions.
- Desktop single-click selection, double-click/hover-button playback, and a current-track marker.
- Mobile single-tap playback from a track through the remaining album, playlist, or visible track results. Playable tracks have no availability label; exceptions use smaller, muted text.
- Space to play/pause and Left/Right to go previous/next when browsing. Focused controls retain their native keyboard behavior; Enter plays a selected track. Previous restarts after three seconds.
- OS media keys and Now Playing metadata through Souvlaki (macOS integrated; Windows/Linux still need platform validation).
- Audio read-ahead defaults to 15 seconds. Settings → General offers a saved 3–60 second buffer; restart the app to apply changes. Larger buffers help cover cellular stalls but use more memory. iOS pauses on output disconnection and audio interruptions, keeping the transport button in sync when returning to the app.
- Audio-output recovery preserves the queue and playback position, pauses on device loss or a default-output change, and reconnects in the background. Press Play to resume; an explicit Play request while disconnected is retained.
- File discovery counts and scan progress, with four parallel tag readers and artwork skipped to reduce network traffic.

Beta qualification is still in progress. The library scan currently refreshes a complete snapshot; incremental per-file caching, progressive filesystem results, filesystem artwork, text marquee, and FFT visualization are not implemented yet.

Supported decoding is determined by the pinned Symphonia version. WAV/FLAC/MP3/Vorbis and CDDA are the first validation targets. AAC/M4A gapless playback and Opus require additional work before they can be advertised as supported. Stereo and mono are supported; multichannel needs an explicit channel mapper. Do not interpret successful playback as verified bit-perfect output.

Current redumper `.scram/.state/.subcode/.toc` audio packages now play directly, including without BIN/CUE files. Raw remains the default; turn off “Prefer redumper raw dumps” and reopen the folder to choose BIN/CUE. With raw selected, invalid packages are reported rather than silently replaced with BIN playback. Missing BIN files can use a supported raw package even when BIN is preferred. Reopen existing cached libraries to replace the old pending-decoder entries.

The initial raw reader supports single-session, two-channel, all-audio discs. Mixed-mode/multisession raw images remain explicitly unsupported. Raw samples retain the recording timeline; drive offset is not applied twice. Skipped/C2-error samples stop playback with an error. CUE playback includes stored and synthetic gaps and excludes data pregaps. Hidden first-track audio is exposed by junk-libs but has no player UI yet. PRE flags are reported in album source text; de-emphasis is not yet applied. Full real-dump comparison is still pending the fixture path.

## Home and libraries

The app opens on **Home**. Choose **Local folder** or a saved server, then choose a
saved folder library or server collection and press **Browse library**. **Open
folder** adds a local library on desktop; **Add / manage servers** opens server
settings. Lists and general settings are also available from Home.

The house icon returns to Home from the browser or album controls. Returning to
the same library keeps your album, selection, search, and scroll position. Playback
controls remain available on Home and in Settings. Cmd/Ctrl+F opens library search
directly from Home.

## Search and artist browsing

Choose **Albums** or **Artists** above the library list. Artists includes complete album-artist credits; track-only artists can be found through search. Opening an artist also associates collaborations containing their complete name, using name boundaries rather than guessing how to split credits. These are textual associations, not authoritative artist identities.

Search matches all query words across the applicable metadata fields, ignoring case and accents. For example, an artist name plus part of an album title can find a track. Artists, Albums, and Tracks each show five results and a total count; **Show all** opens the full list. Search covers the loaded catalog, never additional folders or unconfigured services.

On desktop the field stays above the sidebar; on phone layouts it sits above playback controls in the library view. Cmd/Ctrl+F focuses search. Escape leaves text editing without clearing the query. Back restores the query, selection, and scroll position. Clearing search returns to the selected browser mode while keeping open details. Search is not saved across restarts.

On desktop, selecting a track opens its album and highlights it without playing. Play, double-click, or Enter on a search/artist track plays that song followed by the remaining explicit queue. Album-view playback continues through the album. Where desktop selection replaces its row, pointer navigation waits briefly to distinguish a double-click; keyboard and accessibility selection navigate immediately.

On mobile, tapping a track starts playback immediately and continues through the remaining tracks in the album, playlist, or visible search/artist results, in their displayed order. Unplayable playlist entries are skipped; tapping an unplayable entry leaves playback unchanged. Add to queue does not navigate or start playback.

Existing saved album-artist sorting preferences become Artists browsing automatically. The library cache and provider/media identities remain compatible. Search does not test media availability; playback reports unavailable sources as before.

## Automatic metadata

“Automatic metadata from MusicBrainz” is enabled by default. Public metadata reads require no account or API key. The app identifies complete all-audio CUE/redumper discs by MusicBrainz disc ID. File albums with missing names can use an embedded MusicBrainz release ID, or an exact album/artist search when it returns one release and its track counts, durations and existing titles agree. Fully named file albums are skipped. Unnumbered/unidentified loose files and mixed-mode disc identification are not guessed.

The local library appears before online lookups finish. Disc lookups take priority over ordinary file-album probes. Album status distinguishes unmatched discs, ambiguous editions, and source/network errors. Requests run on a worker with a shared limit of one request per 1.1 seconds, a descriptive User-Agent, a 12-second timeout and bounded responses. The app sends disc/release IDs or album/artist names to MusicBrainz, never audio or full filesystem paths. Disable the checkbox to stop further requests; an in-flight request may finish but its result is discarded while disabled. Reopen the folder to start another lookup pass after enabling it.

Missing album titles, artist credits and track names are filled; existing non-placeholder names win. Matching editions must agree on album identity; when translated editions disagree on track names, only unanimous fields are applied and the album is marked ambiguous. Updated albums show MusicBrainz in their source label. CUE track TITLE/PERFORMER and CD-TEXT names are read locally first. Playback locations, track order and sample boundaries do not change.

Responses, negative results and match provenance stay in the local SQLite database. Positive responses are reused for 30 days, missing results for seven days, and stale cached results remain usable offline. A network failure stops that lookup pass while local playback remains available. No source files are written. The saved library and response cache survive restart. Cover art, genres/year fields, fingerprinting and a manual match/correction UI remain future work.

API rules: [MusicBrainz API](https://musicbrainz.org/doc/MusicBrainz_API). Disc identity: [MusicBrainz disc ID calculation](https://musicbrainz.org/doc/Disc_ID_Calculation).

## Verify

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

See [architecture](docs/architecture.md), [release gates](docs/release-gates.md), and [prototype validation](docs/validation.md).

## junk-libs development

The disc, audio and playback crates use junk-libs revision
`2c8eaf8fd5a7f3e5f4873d3463b8008039e9af98`. The Collector client SDK and server
integration tests use Collector revision `14d56ab4cceaf82c09086d56d987484bab46d618`.
Cargo manifests and the lockfile pin the exact Git sources; Cargo fetches them
automatically, and no sibling checkout or local Cargo patch is required. Server
crates are test dependencies and are not bundled into the player.
The disc API contract and upstream format evidence are in
[junk-libs-disc/PLAYBACK.md](https://github.com/AberrantWolf/junk-libs/blob/2c8eaf8fd5a7f3e5f4873d3463b8008039e9af98/junk-libs-disc/PLAYBACK.md).

## License

Application source: MIT. Slint is used under its Royalty-free Desktop, Mobile, and Web Applications License with the AboutSlint attribution in Settings. Other dependencies retain their respective licenses; distributed packages must include their notices. See https://slint.dev/terms-and-conditions.

A read-only diagnostic for scanner counts, timing and metadata outcomes uses a temporary cache:

```sh
cargo run -p amp-library --example scan_probe -- /path/to/album
```

Collector servers can be added from Servers / Lists. Approve enrollment in the
server interface; provide owner certificate material for private certificate
trust when required. See [connection and validation](docs/collector-connection.md)
for offline downloads, list editing and qualification limits.
