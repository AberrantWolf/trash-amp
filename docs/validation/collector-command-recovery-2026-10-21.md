# Collector list-command recovery — 2026-10-21

Post-alpha.3 source qualification, using an isolated checkout. The operator's
staged native release work remains untouched. Collector dependencies are pinned
to published `59faf341a0141c0424231d1700cfc16a006146a7`, and shared audio to published
`993afc932c6b2acb5cf02c95d3cb8a387d9a70a5`; no local path patches are used.

The adapter stages a command's full server/account identity, collection, list,
operation, expected revision and receipt under its database mutex before HTTP.
An equal pending receipt converges on the saved inputs. Changed inputs fail without
replacing them. Self-review found and repaired the previous generic upsert, which
could replace an unresolved command even though the server would reject its
changed receipt. No additional command history or schema migration is introduced.
The existing Lists retry action reads saved commands after restart; desktop policy
continues to block unrelated list edits while an unresolved command is pending.

The real TLS regression creates an actual Collector archive, imports and prepares
a fixture, enrolls a native client and uses the ordinary adapter submit method.
Test-only response damage occurs **after** the real server has committed: the
normal successful JSON is truncated. The native client reports an uncertain
result and retains the command. Actual server reads establish that creation and
append committed. Changed title, server identity and revision cannot replace the
original pending request. Closing and reopening the local profile, then retrying
its saved request returns the same list/revision and preserves exactly one playlist
and the same single entry identity. Both create and append exercise this path.
No successful server effect is mocked, and no new receipt is generated for retry.

All eight affected adapter tests passed: four unit tests and four actual-TLS
journeys, including existing CD, FLAC/MP3 and game-audio stream/download/offline
regressions. The one existing actual-output-device test remains opt-in/ignored.
Strict all-target Clippy and formatting passed. The only lockfile change adds the
already-resolved Axum test adapter; dependency versions and published pins remain
unchanged. Self-review checked atomic receipt admission, complete input identity,
reopen/retry, actual committed outcomes, pending-state clearing and bounded fixture
teardown. No UI or public API change requires a second playback engine or pipeline.
The named checks qualify native adapter/transport/disk recovery, not a physical
output device, system-trusted second-device enrollment, network route loss or the
remaining stalled/malformed media-range matrix. No native package is released.

## Actual uncached-read revocation — 2026-10-21

A subsequent real-TLS case enrolls a distinct non-administrator Viewer, reads a
complete 256 KiB original-media chunk through the same blocking RangeReader used
by native playback, then coordinates a real permission mutation before its next
uncached byte. One case removes the collection grant; another revokes that native
credential. Both next reads fail with PermissionDenied. The reader/runtime remain
on their worker, coordination and teardown are bounded, and no response or access
outcome is mocked. Existing owner enrollment still uses the same helper.

This qualifies new network reads for those cases, not instant recall of bytes
already fetched, offline DRM or audio-output-device behavior. Explicitly retained
offline copies remain available under the existing product policy. Broader stalled,
changed or malformed-range and physical-output cases remain separate.

All nine affected adapter tests passed after this addition (four unit, five actual
TLS journeys; the one existing physical-output test ignored), with strict all-target
Clippy and formatting. The production adapter and Collector pins are unchanged by
this additional qualification.
