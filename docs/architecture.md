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

Every item includes library, provider and item IDs. Filesystem identifiers initially use canonical paths; renaming/moving files changes identity. Metadata release IDs must never collapse duplicate local copies.

Media access is independent from library enumeration. The current variants are local files, CUE track spans and managed items. Future access adapters must support opaque native handles, ranged HTTP access, sequential-only streams, and availability changes. Do not convert all sources to `PathBuf` or assume seek is supported.

For DLNA: discovery produces providers, ContentDirectory browsing produces album/track records, and resource resolution produces access capabilities. No requirement for a filesystem-shaped remote hierarchy or byte-level access from every provider.

## Apple and Android

Apple native library enumeration and asset access require user authorization and native frameworks. Available unprotected assets can use AVFoundation-backed access where needed. Subscription content uses platform-managed playback; PCM effects and visualization cannot be assumed. A shared playback controller must reconcile native playback events with transport state and capabilities. Crossing between managed and PCM backends does not carry a gapless guarantee.

Android uses MediaStore/content URIs and a native media service. Background playback must not depend on an Activity or Slint window. Media3 can expose shared browsing/transport through a Player adapter and MediaLibraryService.

CarPlay uses native templates and the audio entitlement. Android Auto uses the platform media browsing/session contracts. Neither reuses the custom desktop canvas.

## Audio correctness

Keep sample positions as integers and display time as a projection. The engine keeps output open between compatible tracks; input is prepared ahead through a finite ring. It compensates resampler delay and bounds output duration. Tests currently establish WAV sample/seek behavior and resampling length, not all-format or hardware gaplessness.

CD playback must honor INDEX boundaries, pregaps, HTOA, data-track exclusion and offset semantics. Raw redumper sample alignment and state interpretation belong in reusable junk-libs parsing/reading code, with app policy in this project. Never apply a drive offset twice.

Bit-perfect output is a future goal, not the initial contract. Output uses the default device configuration and conversion when needed. Device loss is currently reported; automatic reopening, route changes, mobile interruptions and exclusive output are future adapters.

## Catalog and metadata

Single active library is development-only. Opening a different library can replace the current cache. Failed scans must not replace it with an empty catalog. Cache stays on local storage; source media is read-only.

Current grouping uses containing directory, album title and album artist, and sorts by disc/track. Future grouping should recognize multi-disc subfolders while preserving distinct editions and copies.

Future metadata order: cached user corrections, trustworthy embedded/CD-TEXT fields, MusicBrainz TOC/release candidates, optional GnuDB fallback, filename fallback. Store provenance, confidence, timestamps, negative results and overrides separately. Artwork uses local art first, then Cover Art Archive. Automatic lookup defaults on but has a preference. Rate-limit providers and never block playback on metadata.

No playlists, equalizer, metadata writing, ripping, transcoding/export, music uploads or curator features in the initial scope.
