# Architecture and accepted direction

Platform order: macOS, Linux, Windows, iOS, iPadOS, CarPlay, Android, Android Auto. Android Automotive OS is a separate potential deployment target.

Slint is the initial custom UI, using Skia. Layout follows available width, height and text scale. Native platform capabilities still govern safe areas, input methods, accessibility, menus and lifecycle. A future native UI must be able to replace Slint without replacing browsing or playback logic.

## Boundaries

- `amp-core`: serializable library/album/track identities, media access descriptions, preferences and provider contract. No Slint, database, device or filesystem APIs.
- `amp-library`: filesystem enumeration, tags, CD structure and local cache. Introduce additional providers here without requiring every source to have a path. UI adapters should eventually consume paged queries and incremental updates.
- `amp-audio`: playback commands, stream preparation, decoding, seeking, format conversion and output. Bounded sample buffering isolates the audio callback. Device lifecycle will become a separate output adapter; CPAL is the first implementation.
- `trash-amp`: desktop composition and presentation adapter. It currently hosts application orchestration, which must migrate to a shared service before mobile/native bindings are added.

The current implementation establishes these boundaries but does not yet implement the full future service/driver system. UniFFI is the planned Swift/Kotlin command and event bridge. Raw audio should remain in the engine, not flow through the UI bridge.

## Identities and sources

Every item includes library, provider and item IDs. Filesystem identifiers initially use canonical paths; renaming/moving files changes identity. Complementary tagged album fragments can merge across artist folders. Conflicting disc/track slots and distinct explicit album artists keep copies separate. Metadata release IDs alone must not collapse duplicate copies.

Media access is independent from library enumeration. The current variants are local files, CUE track spans and managed items. Future access adapters must support opaque native handles, ranged HTTP access, sequential-only streams, and availability changes. Do not convert all sources to `PathBuf` or assume seek is supported.

For DLNA: discovery produces providers, ContentDirectory browsing produces album/track records, and resource resolution produces access capabilities. No requirement for a filesystem-shaped remote hierarchy or byte-level access from every provider.

## Apple and Android

Apple native library enumeration and asset access require user authorization and native frameworks. Available unprotected assets can use AVFoundation-backed access where needed. Subscription content uses platform-managed playback; PCM effects and visualization cannot be assumed. A shared playback controller must reconcile native playback events with transport state and capabilities. Crossing between managed and PCM backends does not carry a gapless guarantee.

Android uses MediaStore/content URIs and a native media service. Background playback must not depend on an Activity or Slint window. Media3 can expose shared browsing/transport through a Player adapter and MediaLibraryService.

CarPlay uses native templates and the audio entitlement. Android Auto uses the platform media browsing/session contracts. Neither reuses the custom desktop canvas.

## Audio correctness

Keep sample positions as integers and display time as a projection. The engine keeps output open between compatible tracks; input is prepared ahead through a finite ring. It compensates resampler delay and bounds output duration. Tests currently establish WAV sample/seek behavior and resampling length, not all-format or hardware gaplessness.

CD playback uses junk-libs `CueAudioDisc` and `RawAudioReader`, whose public positions use integer stereo sample frames. The CUE playback timeline preserves indexes and inserts synthetic silence without changing the older hashing APIs. Current all-audio redumper images use the raw recording timeline and TOC boundaries; drive read offset is already applied. Raw readers accept `Read + Seek + Send` sources for future range-backed access. The filesystem adapter selects the representation and stores a distinct `MediaAccess::RedumperTrack`; the audio adapter selects strict rejection of unrecovered samples. PRE is retained as metadata, with de-emphasis still pending. The raw reader deliberately rejects automatic mixed-mode/multisession playback pending reliable Q/index reconstruction. Hidden CUE audio has a library range API but no UI yet.

Bit-perfect output is a future goal, not the initial contract. Output uses the default device configuration and conversion when needed. Device loss is currently reported; automatic reopening, route changes, mobile interruptions and exclusive output are future adapters.

## Catalog and metadata

Single active library is development-only. Opening a different library can replace the current cache. Failed scans must not replace it with an empty catalog. Cache stays on local storage; source media is read-only.

Current grouping first gathers tracks by folder, tagged album title and explicit album artist (never by individual track artist). It then merges same-title tagged fragments across folders when their explicit album artists do not conflict and their disc/track slots do not overlap. Missing track numbers fall back to track-title conflicts. Mixed track artists display as Various artists unless an album artist is specified. Untagged folder names alone do not justify cross-folder merging. This deliberately permits title-based fallback for artist-first collections, with some ambiguity when unrelated albums lack identifying tags. Tracks sort by disc/track. Reopening a library rescans with this rule; cached snapshots retain their previous grouping until rescanned.

Metadata currently fills placeholders after local tags, CUE TITLE/PERFORMER and CD-TEXT. `amp-library::metadata` owns MusicBrainz query construction, exact audio TOC IDs, release/medium matching, response caching and provenance. It uses a bounded blocking HTTP client exclusively on a background worker. Disc-ID candidates must agree on names; file name searches require a single exact release/artist hit plus complete track counts and duration/title agreement. Embedded release IDs select medium positions explicitly. Fully named file albums need no requests.

The local scan publishes first. A cache pass runs before the network pass; an atomic preference disables network access, and scan generations discard stale callbacks when switching libraries. The UI changes individual rows and selected-track labels without changing playback order/access. HTTP responses live in `metadata_http` with positive/negative expiry; per-album matches live in `metadata_matches`, separate from the library snapshot. Stale cache results remain available offline. Snapshot persistence occurs at completion; the response cache also allows interrupted enrichment to resume on restart. Existing non-placeholder values are preserved, including values corrected in the cached snapshot. A dedicated user-override editor/model is still future work.

Providers should remain independent of UI/platform library access. Future work includes GnuDB fallback, manual candidate selection, explicit overrides, additional descriptive fields and artwork (local art first, then Cover Art Archive). No audio uploads or source-file writes are involved.

No playlists, equalizer, metadata writing, ripping, transcoding/export, music uploads or curator features in the initial scope.

## Catalog search and browsing

`amp-core::browse` owns a provider-independent in-memory catalog index, normalized query matching, complete artist-credit identities, and artist-page associations. Accent-insensitive lookup does not collapse separately spelled artist identities. Artist browsing includes album credits only; search includes track credits too. Source paths and media access never participate in matching.

The desktop browsing adapter keeps navigation history, query/section state, row-to-ID actions, and the initiating track playback context separate from album storage order. Search workers run after a 100 ms debounce, and query/catalog generations reject outdated replies. Metadata events are batched before rebuilding the index and refreshing the active view. Library replacement resets navigation; failed scans do not.

The persisted `browse_artists` preference accepts the former `sort_by_artist` field when reading old settings. Search state is transient. Explicit queue tails remain separate from automatic album continuation when a search or appearance track starts playback.

## Collector adapter

See [Collector connection](collector-connection.md). `amp-core` contains neutral
source coordinates and availability only. `amp-collector` owns HTTP, Keychain,
SQLite snapshots, artwork, receipts and explicit download mechanisms. `amp-audio`
accepts an injected seekable media resolver and cancellation handle, retaining
the shared junk-libs decoder and CPAL pipeline. Desktop browsing generations
are independent of playback and download lifetimes.

## Collector game-disc audio

Collector listening catalogs can include Games entries with recorded CD audio.
The adapter keeps the original game identity and source-specific track positions;
only server-admitted audio derivatives use the existing ranged stream/download
and offline cache path. Native playlist creation explicitly requests audio tracks,
so a Games collection's default inventory lists stay separate. No game importer,
PCM parser or second playback engine is introduced in Trash Amp.
