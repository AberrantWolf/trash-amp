# Collector connection

Trash Amp has an `amp-collector` adapter for authenticated Collector browsing,
revisioned list commands, authenticated seekable FLAC streams and explicit
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
and includes a compact status line.

Credentials are stored through macOS Keychain. Enrollment supports system HTTPS
trust and explicit owner PEM/pairing material; catalog and media share that trust
policy. SQLite snapshots are isolated by server/account, published only after all
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
