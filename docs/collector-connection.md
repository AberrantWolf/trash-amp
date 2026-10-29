# Collector connection

Trash Amp has an `amp-collector` adapter for authenticated Collector browsing,
revisioned list commands, authenticated seekable FLAC/MP3 original and prepared FLAC streams and explicit
downloads. Select one remembered server/account for browsing. Switching selection
preserves the player and queue; each remote track retains immutable originating
source coordinates. Filesystem browsing remains available. Collector metadata
bypasses MusicBrainz enrichment.

The desktop toolbar combines source and collection selection with Refresh, Open
folder, Lists, and Settings. Settings is a dedicated page with General, Servers,
and Downloads tabs. Add, reconnect, or remove a server under Servers; change the
audio storage limit or retry/cancel downloads under Downloads. Lists has its own
page for playback, editing, and list downloads. Done or Escape returns from
Settings, and Library or Escape returns from Lists. Search (Cmd/Ctrl+F) returns
to the library from either page. The playback bar remains available on all pages
and includes a compact status line. A small connection icon at the bottom-right
shows disconnected, public streaming, trusted LAN/private streaming, or direct
HTTPS internet streaming. Hover,
click, or focus and press Enter/Space for details; click again or Escape dismisses
pinned details. Connected servers with an unconfirmed or unavailable streaming
route have an explicit explanation. The selected server is checked after refresh
and every 30 seconds on a worker using authenticated metadata and a media HEAD
request, without downloading or preparing audio. Local folders and removed
connections show disconnected. Results from previously selected servers are ignored.

The direct-internet icon is enabled after a successful media HEAD through the
shared client's nonlocal route policy and an authenticated direct-HTTPS
advertisement. The client prefers reachable private media, then direct HTTPS,
then explicitly permitted public-origin fallback when no direct endpoint is
configured. An advertised HTTPS endpoint that fails trust/reachability does not
fall back to the public control origin. Private includes any reachable trusted
private route supported by the client; it does not prove physical LAN membership.
Empty libraries cannot verify streaming until a playable track exists.

Credentials are stored through macOS Keychain. Enrollment supports system HTTPS
trust and explicit owner PEM/pairing material for control. Private media uses
authenticated certificate pins; direct internet media uses system HTTPS roots and
short-lived, target-scoped grants instead of the enrolled control credential. SQLite snapshots are isolated by server/account, published only after all
pages succeed, and restored immediately. Generation fences reject obsolete
refresh results. A profile lock prevents concurrent writers. Lists use server
expansion, typed edit permissions, expected revisions and durable input-bound
command receipts; pending commands can be retried or explicitly discarded.

Explicit download requests freeze tracks before admission, reserve a configurable
10 GiB global audio budget, and persist bounded work for recovery, cancellation
and retry. Length and SHA-256 are checked before atomic publication. Completed
files remain available offline after removing credentials or connections; explicit
removal deletes retained audio. Streams are not retained. Artwork has a separate
128 MiB cache. Neither browsing nor playback requests server preparation.

Cancel marks every unfinished download in the active batch as cancelled and keeps
completed audio. Interrupted pending work resumes at launch; cancelled work needs
an explicit retry. Album disc choices use Disc N labels and retain their selection
by source identity without filtering the stored catalog. A selector appears only
when there is more than one source disc; alternatives for the same disc number
use Version N labels. Browsing artwork uses shared, bounded thumbnails decoded
in the background as rows are requested.

## Validation, 2026-10-07

Workspace tests cover cache isolation across server/account identities, restart
selection, atomic budget admission, retained audio integrity and command receipts.
A temporary real TLS Collector fixture uses actual BIN/CUE import and FLAC
preparation, native enrollment, two servers with colliding item IDs, intentional
list duplicates, authenticated streaming decoding, downloads, credential removal
and offline seek/decode. The four-second PCM fixture decodes to 176,400 frames.
A longer audio regression covers resampler draining without sustained overflow.

The actual macOS Slint app was exercised with temporary server/profile data for
enrollment, Keychain restoration, album artwork, list creation/append, stream
playback and explicit download. The first synthetic ramp sounded low/buzzy;
subsequent fixtures use silence. Hearing that signal does not validate fidelity.
Output capture/bit accuracy, device-loss behavior, the full stalled-range and
revocation matrix, rapid-switch UI automation, two-account live authorization
isolation and comprehensive download failure/restart UI journeys remain release
gates. Mobile qualification, discovery, failover, certificate rotation and new
codecs are deferred.


## Digital purchases — 2026-10-19

Collector file sources carry native format/timing and immutable package-file
identity without a conversion profile. The adapter prefers playable originals,
uses their media type for decoder hints, calculates duration at the native rate,
and keeps printed numbering separate from source positions. Album refresh honors
explicit alternate-encoding selection. Download identity retains account/server,
source, actual file index and digest, so two tracks in one original package remain
separate. Retained artwork/text and FLAC/MP3 audio reopen offline after server or
credentials are removed; purchase/order fields are absent from this snapshot.

Collector and junk-libs use exact published Git revisions in Cargo manifests and
lockfiles; local patches and sibling checkouts are no longer required. Real TLS streaming/decode/seek/download/offline and actual
CoreAudio callback checks pass; see Collector's
`docs/validation/digital-purchases-2026-10-19.md` for the full matrix. The actual
native GUI purchase journey also passed on this Mac: actual FLAC/MP3 streaming,
seek/next/queue, album artwork and download, responsive connection removal, and
server-stopped offline reopen/search/artwork/playback/seek with both originals.
Descriptive artist/queue captions keep immutable source IDs internal. Credential
removal runs on a worker with a visible pending state, so an OS Keychain wait does
not stall the UI. Temporary profiles and servers were used throughout. Broader
LAN/device-loss/revocation qualification remains in Collector's HL-04 queue.

## Automatic private playback — 2026-10-22

Collector's authenticated control connection now supplies private media endpoints
and short-lived certificate pins to the shared native client. Both streaming and
explicit downloads use this discovery through `media_reader`; playback does not
replace the enrolled server/account or change offline-cache identities. No OS
certificate import, private DNS, manual LAN URL or listener-side setup is needed.
The server operator must allow the media listener's TCP/UDP port on their private
network (default 9845); it is media-only and is not a management listener.

Only a pinned listener reporting the enrolled server identity receives the native
credential. Every range checks current authorization. Connection failure refreshes
routes and retries the same immutable range once; changed validators, denied
access and cancellation stop playback. The server's explicit media policy controls
fallback; the current default disables public media fallback. Already downloaded
tracks retain the existing offline behavior. Generic reachable private addresses
may work over an existing OS route, but no overlay provider is inspected or managed.

The prior `collector-lan` URL is a personal stopgap, not automatic detection. Real
TLS integration fixtures now disable media on the control origin and exercise the
actual FLAC decoder, seeks, downloads, revocation and retained offline playback
through a distinct automatically trusted private listener.

The delivery pins Collector client/server source `14d56ab4cceaf82c09086d56d987484bab46d618`.
The new server and client builds must be installed together; the production NAS
and work Mac are not changed by this source delivery. Initial private-route
discovery needs the normal control connection; retained downloads work offline.


## Direct HTTPS playback — 2026-10-24

The adapter now pins Collector `c5587138c90f5d460b371cb9e489c8eb28aedb7a`,
including the shared native client's direct HTTPS media support. Outside a
reachable private route, streaming and explicit downloads automatically use the
server-advertised HTTPS media service. No extra URL or listener setting is needed
in Trash Amp. The server must already have direct media configured and reachable
with a certificate trusted by the client system. Account identity, queue source
coordinates, cancellation, immutable range validation and offline storage identity
remain the same across routes.

The connection indicator distinguishes direct HTTPS from public-origin streaming.
The shared reader currently exposes only whether the verified route is private;
for a successful nonlocal HEAD, the adapter reads the bounded authenticated route
advertisement to distinguish direct HTTPS from permitted public fallback using
the shared client's selection policy. If that descriptor cannot be read or its
server identity does not match, the indicator stays unconfirmed. As before, this
is a periodic capability check for the selected server, independent of cached
playback or a queue originating from another server.

Real TLS fixtures exercise direct HTTPS FLAC decode and seek, explicit downloads,
credential removal and retained offline playback. A separate subprocess trusts
only the fixture's media certificate, leaving OS trust unchanged. The matrix also
covers private-route preference and rejection of an untrusted advertised media
endpoint even with public fallback permitted. This is local integration coverage;
actual external-network listening is not qualified by these fixtures.
