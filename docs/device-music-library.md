# Device Music Library

Home offers Device Music Library on macOS, iOS and iPadOS. It browses the Music
app's library as a separate source. Local, unprotected songs play through Trash
Amp's existing queue, PCM conversion and output engine. No Apple Music
subscription is required. Cloud-only and DRM-protected songs remain visible with
an explanation; they cannot be played or queued. This does not import playlists,
change the Music library, download cloud content or provide subscription playback.

Select **Home → Music source → Device Music Library**, or choose **Settings →
Servers → Open Device Music Library** to open the same source flow. The Servers
dropdown itself lists only Collector servers. Select the source to request permission. Startup and lifecycle refreshes never
request permission. Home shows loading, empty and permission failure states and
provides a Music access settings action. Refresh rereads the library; native
library changes and foreground activation revalidate the active source. iOS
library notifications received while backgrounded defer work until foreground.
Other sources can continue playing while browsing this library, and vice versa.

## Ownership and native boundaries

`amp-app::catalog` owns registered sources, selection, restoration, asynchronous
loads and acceptance. The same worker serializes catalog and selection writes.
`amp-app::sources` defines opaque stable source IDs, descriptors, access status
and the injected source provider interface. Hosts register providers before UI
startup. Folder support is a capability; iOS does not offer a folder source.
Collector's cached/live source adapter lives in `amp-collector`, independently
of its Slint connection/list presentation. Enrollment and list-mutation workers
request their follow-up refresh through the catalog owner.

The `active_source` state key is authoritative. When it is absent, registration
uses the legacy Collector selection, otherwise the last folder. Selecting a
folder writes the folders marker and retains its existing active-folder ID.
Collector's own catalog/download cache remains authoritative for its content;
its legacy selected-connection field is only a migration input. Apple snapshots
and artwork references use provider-scoped state keys, never `folder_libraries`.
Source selection commits independently of loading: choosing an empty, unavailable
or permission-denied source keeps that source selected and shows its loading,
empty or error state. Previously saved catalogs remain stored; valid cached data
for the chosen source survives a failed refresh. Revoked authorization removes
exposed Apple metadata and cached artwork and revokes that provider in playback.
Selection generations and per-load request tickets reject late source and artwork
completions. Notifications during a load coalesce into a follow-up refresh.
Automatic refreshes retain the selection generation, so they cannot cancel an
enrollment or list mutation. Selection writes are compensated if cancellation
wins during persistence. Download changes reproject Collector's cached catalog
without a network request; connectivity is explicit source state rather than
inferred from which tracks happen to be downloaded.

`amp-apple-library` contains all Apple framework calls. macOS enumerates
`iTunesLibrary` and resolves item IDs to their current local file locations.
Security-scoped URL access is retained for the file reader's lifetime. The current
Mac distribution remains nonsandboxed; a sandboxed distribution must additionally
supply the music-library entitlement. Music authorization uses the public
Objective-C StoreKit bridge on Mac, deprecated in favor of Swift MusicAuthorization;
that compatibility choice is confined to this crate.

iOS/iPadOS enumerate MediaPlayer music items and resolve persistent IDs to
AVFoundation assets. `ipod-library` URLs remain opaque. A dedicated native worker
owns AVAssetReader and returns bounded float PCM chunks through a sendable reader
handle. Frame-based seeks recreate the asset reader and trim using sample-buffer
timestamps. Cancellation releases application waits independently of native API
completion; closing the reader disconnects its mailbox and releases native objects
when the outstanding call finishes. No native object or raw audio passes through
Slint. AVAudioSession and output remain owned by the existing audio worker.
The native asset decoder is separate from MediaPlayer enumeration so generated
files can exercise it on Mac. Mac library initialization also uses a cancellable
wait, and cancelled file reads return terminal errors instead of retry signals.

Hosts install an immutable `amp-audio::ResolverRegistry` before playback. Managed
items resolve by provider, not current browsing selection. Resolver input can be
encoded bytes or a decoded `PcmReader`; disc/file/native decoders share bounded
read, format and seek handling. Playback eligibility is enforced in `amp-app` for
all callers. Album/default Play skips blocked songs, explicit blocked-track
requests preserve existing playback, and filtered lists retain order. Resolvers
recheck actual access when opening queued media. An item removed after enqueue
uses the normal playback-error path; it does not trigger a cloud fetch.
Transport commands fence cancellation against command publication. A read or
open abandoned by Seek or a queue edit preserves play intent and does not report
a decoder failure. Accepted metadata also updates idle Play eligibility without
depending on a UI tick.

Artwork loads after metadata on a job thread, is constrained to 512 pixels, and
uses the existing UI/Now Playing artwork paths. Artwork failure does not prevent
playback. Native album IDs preserve separate copies and compilations; native
metadata is not passed through folder title-based album merging or MusicBrainz.

## Validation and remaining qualification

Native Mac tests use generated WAV/AAC/ALAC files with the same AVAssetReader
implementation as iOS. They check bounded reads, channel/rate metadata, nonzero
seek, EOF and AAC priming/padding. These tests need host CoreAudio codec services;
a restricted execution sandbox may prevent decoding even generated files.
In that environment, `afconvert` can report that `aac ` is an unknown format,
and AVAssetReader can fail with AVFoundation error -11800 and underlying OSStatus
-1, including for WAV input. These messages alone do not establish invalid
fixtures or decoder settings. Compare `/usr/bin/afconvert -hf` inside and outside
the restriction: AAC/ALAC disappear from its available formats when codec service
access is blocked. Rerun the native tests with host service access rather than
ignoring them or changing the requested audio format:

```sh
cargo test -p amp-apple-library --lib native::asset::tests -- --nocapture
```

Run the suite with access to those services:

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p trash-amp-ios --target aarch64-apple-ios-sim --locked -- -D warnings
scripts/bundle-macos.sh --debug
scripts/bundle-ios.sh --simulator
scripts/bundle-ios.sh --device
```

Headless tests cover provider restore/cache/failure, notification-based access
revocation, late completions after folder selection, provider-aware resolution,
decoded PCM conversion, blocked queue requests and revocation against the actual
audible track. Compatibility tests cover older serialized tracks without blocked
reasons. The two bundles include NSAppleMusicUsageDescription.

Physical synced-library permission/enumeration/playback remains a release gate.
Qualify signed Mac, iPhone and iPad builds with MP3/AAC/ALAC, protected/cloud-only
items, large libraries, compilations, artwork, resync/removal, denied/restricted
and revoked access, locked-device transitions, scrubbing, interruptions,
headphone/Bluetooth route changes and foreground return. Check iPad rotation,
multitasking and VoiceOver. Simulator builds and generated-file native tests do
not establish device-library access, gaplessness or sample-perfect hardware output.

Validation on 2026-10-29: 148 workspace tests and 28 release-tooling tests pass;
three existing fixture/live-service tests remain ignored. Workspace/all-targets
and iOS simulator Clippy pass with Rust warnings denied. Generated-file native
WAV/AAC/ALAC tests pass with host codec services. Mac, simulator and unsigned
device bundles build; the Mac and simulator signatures verify. The generated
notices include the image/TIFF dependencies. The actual Slint source panel was
rendered at phone and tablet dimensions using an isolated profile and
noninteractive source restoration. Deprecated Slint viewport aliases have been
replaced, and the workspace builds without warnings. These checks do not qualify
a physical synced library.
