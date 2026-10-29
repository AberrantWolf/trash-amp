# Application services

`amp-app` runs playback and catalog services without depending on Slint, Souvlaki
or an OS API. Their explicit owners live in the platform launcher. The playback
worker accepts commands from a cloneable `Playback` handle.
The same handle serves UI actions, system media commands and future native
interfaces. Creating the service does not open an audio device or activate a
platform audio session.

The service owns the active sequence, the explicit up-next queue, the selected
album used when Play starts an idle session, and the playback engine. Album
continuation remains distinct from explicitly queued tracks. Starting a search
or appearance track retains only the remaining explicit queue. Playing a saved
list replaces the previous queue. Clearing up next preserves the current track
and automatic album continuation.

The worker consumes engine events and reads its clock independently of any view.
It publishes a small presentation snapshot containing the current track, explicit
queue, clock, output state and latest status notice. The queue snapshot is shared
until it changes. The UI can stop reading without stopping playback commands,
engine-event handling or native metadata updates. Commands are asynchronous;
presentation snapshots can briefly lag a UI action.

Engine sequence replacements carry an acknowledgement. The service waits for the
matching sequence before interpreting an engine track index against a replacement
list. This prevents an old sequence's clock from temporarily naming the wrong
track after an album or list change. Failed native activation preserves the
previous sequence and queue so the request can be retried.

`PlaybackService` is the explicit lifetime owner, held by the platform launcher.
The UI receives only a command/snapshot handle. Dropping it stops and joins the
orchestration worker and releases its engine. Command handles retained by native
callbacks cannot restart a stopped owner. A future host can own the service for
its application or media-service lifetime rather than a particular screen.
`with_engine_factory` provides the backend seam for alternative playback engines.

## Native media sessions

`amp-media` translates native transport events into application commands. Native
media objects remain on their installation thread. The host supplies a dispatcher
for metadata publication: desktop uses the event-loop dispatcher, while iOS uses
Apple's main dispatch queue. The service publishes track/state changes and clock
updates at whole-second boundaries. It never waits for a UI timer to forward them.

The adapter retains at most one pending dispatch task and the newest metadata.
If the main thread is inactive, updates replace the pending value instead of
building an unbounded backlog. Dropping the media-session guard detaches native
commands. Already queued tasks check the session identity before updating an OS
object.

The host also supplies session preparation. The application worker calls it
before engine creation, sequence replacement and explicit resume requests. The
current hosts use a no-op hook: `amp-audio` serializes iOS session activation and
deactivation with device operations on the audio worker. Pausing releases the
iOS stream and session, retaining a bounded PCM buffer and dormant decoder state
for resume. Preparation must not synchronously wait for the UI thread. Now
Playing observers should schedule work and return promptly.

## Catalog completion and persistence

The catalog owner also owns registered source selection, startup restoration,
cache/live loads, access status and refresh. Hosts supply providers through
`amp-app::sources`; Slint sends commands and projects immutable snapshots.
Collector source jobs and Apple library jobs use the same generation and
selection-persistence boundary. See [Device Music Library](device-music-library.md).
Provider resolvers are registered in the launcher before playback starts, and
can return encoded bytes or decoded PCM without selecting another engine.


The launcher also owns `CatalogService`. Its worker holds the live local SQLite
connection. Folder scans and metadata lookups run on separate job threads, so
slow files or network requests do not block catalog commands or playback.
The UI reads immutable catalog snapshots and renders progress and notices.
It does not acknowledge completion to make a job finish or persist its results.

A shared generation scope covers local and external source selection operations.
New selections cancel obsolete jobs immediately. Per-load request tickets fence
automatic refreshes within a selection generation; notifications during a load
coalesce into another refresh. These refreshes do not cancel concurrent enrollment
or list mutations. The owner rejects stale scan results, metadata patches and
provider completions before they can replace the catalog.
Local and external catalog sources remain distinct. An opaque provider catalog
is never persisted as a filesystem folder just because it has a nonempty root.

Metadata refreshes use `save_library_snapshot`, which does not change the active
folder. Accepted patches are flushed when enrichment finishes, before a source
switch, and on service shutdown. This preserves completed metadata work while
preventing an old lookup from selecting a previous folder. Catalog publication
batches metadata events to limit copies of large catalogs.

Collector workers publish completed catalogs through the same owner. Selection
persistence runs there before an acceptance receipt is returned. Selection writes
return a compensation callback. If cancellation arrives during persistence, the
owner restores the previous saved selection and rejects the completion. A short
acceptance lock orders publication against generation advancement; it never wraps
persistence or compensation. Local folder selection follows the same rule. Enrollment and
successful list mutations start their next refresh on a worker; they do not wait
for the UI to consume a completion event. Provider budget and receipt writes
also use the persistence queue. The UI still projects provider snapshots into
collection/list controls and may read provider caches for those views.

Preference writes are serialized by the catalog worker. Disabling network
metadata updates its cancellation flag immediately, even if a database operation
is still pending. Catalog replacements refresh names in the playback service
without changing media references or queue order. Source changes clear idle-play
selection from the previous catalog. Same-source updates replace idle selection
eligibility by item identity. Existing active playback continues.

Startup loads an initial snapshot before a window is shown. After startup,
presentation callbacks send asynchronous commands. Shutdown cancels outstanding
jobs and joins the catalog owner. A blocked filesystem or HTTP call can finish
later, but its completion cannot commit a catalog through a stopped owner.
Response-cache writes may finish on the lookup worker's separate connection.

## Validation and remaining boundaries

Headless tests use an injected engine and run no Slint loop. They cover transport
and track transitions, explicit queue rules, list replacement, failed activation,
ambiguous default albums, late engine snapshots, metadata-only updates, engine
notifications and owner shutdown. The native adapter has a test for coalescing
updates while dispatch is stalled. Queue edits run on the audio worker against its audible position, after earlier
transport commands. Tests hold the application worker after polling, advance the
engine, and verify that clearing preserves the current queued track and choosing
a new track does not repeat it. The audio recovery suite checks pending transport
ordering and sequence acknowledgement while output is disconnected. A
nonblocking render guard prevents the device callback from changing the audible
position during an edit; the callback emits silence instead of waiting. A gated
regression verifies both clock stability and immediate callback completion.

Catalog tests run with no Slint loop and verify scan/enrichment persistence,
preference writes, rejected stale completions, failed scans, accepted-patch
flushes, opaque provider catalogs and provider selection failures. They also
check that catalog changes update playback names without replacing media and
cannot start an idle selection from a previous source. The UI tests retain saved
folder selection and presentation coverage.

Search navigation, collection/list projection, connection-status presentation
and native dialogs remain presentation concerns. Incremental/paged catalog
queries and native background job scheduling remain future work.

This service does not grant background execution time. iOS interruption policy,
audio-session deactivation, suspended downloads and
hardware playback still need platform qualification. The iOS host now supplies
background-accessible credentials; locked-device behavior still needs a device check.
