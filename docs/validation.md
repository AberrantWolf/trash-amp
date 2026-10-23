# Prototype validation

Automated tests cover exact decoded WAV samples, seek offset, resampling length including short/partial chunks, SQLite settings/library replacement, cancellation, and unavailable roots.

Manual procedure:

1. Run an optimized build with `--demo`. Resize between 980×680, 390×740 and 640×280. Select albums, browse tracks, open settings and enable reduced motion. Repeat with text scaling.
2. Open a real single-album fixture and a mounted multi-album library. Test start, pause, resume, seek, next and previous. Verify no music-folder writes.
3. Use known contiguous WAV/CD tracks and loopback capture to validate that joining tracks neither inserts silence nor drops/repeats samples. Listening alone is insufficient.
4. Repeat seeking while paused and under network delay; test a disconnected share and an unavailable audio device.
5. Compare CPU, memory, frame timing and energy with idle/playing, visible/hidden, animation enabled/disabled, using release builds. Report measurements with hardware and window dimensions.

The generated demo exercises layout only. The level meter is real peak amplitude, not an FFT spectrum. The prototype does not yet satisfy the full performance/mobile validation gate.

## Results from the first macOS development build

- `cargo test --workspace`: 7 tests passed, including exact WAV decoding/seek, resampler length, continuous two-track worker output, CUE track boundaries and within-sector seeking, SQLite roundtrip/replacement, cancellation and unavailable paths.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all --check`: passed.
- Native Skia renders inspected at 980×680, 390×740, 640×280 and 800×700 with 1.5× text. Fixed the narrow-layout switch and header sizing found during inspection. Screenshots in `screenshots/` use generated data.
- `cargo run -p amp-audio --example output_probe`: the default macOS device's output clock advanced to 1.013 seconds with zero callback underruns in the short silent probe. This checks device plumbing, not output fidelity or long-running reliability.

Not yet measured: optimized animation frame time/energy, real network-drive performance, actual compressed-album gaplessness, or mobile behavior. No claim of a completed GUI performance comparison.

Renderer snapshot command (development utility):

```sh
cargo run -p trash-amp -- --demo --select-first --size 390x740 --snapshot /tmp/trash-amp.png
```

## Initial usability fixes

- 12 workspace tests pass; formatting and Clippy with warnings denied pass.
- The transport slider regression sends real pointer events through Slint's testing backend, then verifies that subsequent clock updates and restart positions still reach the thumb after repeated scrubbing.
- Tagged WAV fixtures verify compilation merging across artist-first paths and within one folder, preservation of conflicting copies/explicit album artists, and separation of untagged folders.
- Native renders inspected for matching SVG transport icons and the preferences footer at 980×760 and 640×360. The attribution stays fixed-size at the panel bottom when it fits, and below the scroll viewport when it does not.
- Reopen the library to refresh an existing cached snapshot with the new grouping rules.

## Disc playback implementation

- Added synthetic raw-versus-BIN comparison through the player's actual reader/resampler path, covering both tracks, repeated starts, frame-level seeks, EOF and C2 rejection.
- Added filesystem-provider tests for raw-only discovery, avoiding duplicate raw/CUE albums, raw/BIN preference and explicit invalid-raw errors. Reopen cached libraries after updating.
- junk-libs has separate tests for stereo ordering, exact seeks, signed-range bounds, source truncation, quality states, TOC validation, stored/synthetic CUE gaps, hidden first-track ranges, and stopping before data pregaps.
- Real redumper full-disc comparison is registered as an ignored test requiring the documented Octopath fixture prefix. It has not yet been run; no claim of verified real-dump or captured-device gaplessness is made.
- The sibling junk-libs workspace's existing lockfile requires unrelated dependency resolution, so package tests/clippy were run from a temporary mirror with an independently resolved lockfile. Its tracked lockfile and unrelated crates were left unchanged. Player tests use the actual sibling source.

## Keyless metadata

- The actual Rust HTTP client successfully queried MusicBrainz's documented `I5l9cCSFccLKFEKS.7wqSZAorPU-` disc and matched Nevermind/Nirvana with all 12 track names. The opt-in `live_documented_disc_lookup_and_names` test is ignored during normal offline test runs.
- Deterministic tests cover the published disc-ID vector, CUE initial synthetic pregaps, mixed-mode exclusion, edition consensus/ambiguity, wrong media/track counts, duplicate positions, strict search duration/title matching, preservation of existing names and playback access, positive/negative/stale caching, offline reuse, in-flight cancellation, failures and unchanged media-file contents.
- Desktop enrichment runs after the local scan and on cached-library startup. New scan generations reject stale updates; individual rows and current queue labels update without replacing playback positions.
- Builds use the user's normal Cargo home; no temporary Cargo-home override is used for this change.

## Library search and artist browsing

- 42 workspace tests pass; the existing live MusicBrainz smoke test remains ignored. Added coverage for accent/cross-field matching, whole artist credits and name boundaries, grouped appearances, old preference compatibility, ID-based result actions after sorting, Back and clear-search behavior, metadata refresh, stale worker replies, and explicit playback queue composition.
- Slint regression tests exercise text editing versus playback keys, Cmd/Ctrl+F after returning to the phone library, delayed pointer navigation, detail-pane width, and full 100,000-track results. The large-result test checks that fewer than 100 row delegates exist both initially and after scrolling.
- Optimized synthetic measurement on this development Mac: 10,000 albums / 100,000 tracks indexed in approximately 204 ms; a multi-word search took approximately 11 ms. These are a single local benchmark, not a cross-device latency guarantee.
- Actual native Skia snapshots inspected at 980×760, 390×740, 800×700 with 1.5× text, and 640×280. Inspected grouped search, album-artist browsing, and a page with many one-track appearances. Corrected artist-label alignment, a Back-button constraint that narrowed the detail pane, and unnecessarily repeated album titles in appearance rows.
- GUI QA uses generated tracks and a temporary SQLite directory. New playback behavior is checked through sequence composition without requiring an audio output device; this does not claim new physical-device or mobile-platform validation.

Reproduce search and appearance renders (the demo has no playable audio):

```sh
TRASH_AMP_DATA_DIR=/tmp/trash-search-qa cargo run -p trash-amp -- --demo --search late --size 980x760 --snapshot /tmp/trash-search.png
TRASH_AMP_DATA_DIR=/tmp/trash-search-qa cargo run -p trash-amp -- --demo --demo-appearances --browse-artists --artist "The Late Hours" --detail-scroll 530 --size 390x740 --snapshot /tmp/trash-artist.png
cargo test -p amp-core --release ten_thousand -- --nocapture
```

The QA switches `--search`, `--browse-artists`, `--artist`, `--demo-appearances`, and `--detail-scroll` affect the current demo session; they do not save browser preferences.

## Collector integration, 2026-10-07

See [implementation and coverage](collector-connection.md). Workspace tests and
Clippy with warnings denied passed. The actual macOS app restored its server
selection and artwork after restart, retained album/list selection after refresh,
and retained cached lists while disabling mutations after server shutdown.
Native FLAC playback and explicit download completed. The audible synthetic ramp
is not music fidelity evidence; output capture remains required. Collector
workspace/architecture/Clippy/web gates and browser smoke passed, and phono-junk's
GUI consumer compiled without source edits.

## macOS icon and bundle, 2026-10-07

The app embeds its generated 1024px PNG window icon and bundles a multi-resolution
ICNS through `scripts/bundle-macos.sh`. The debug bundle was built and opened as
“Trash Amp” using an isolated temporary profile; no audio was played. `plutil`
validated its Info.plist and `codesign --verify --deep --strict` accepted the
ad-hoc signature. Formatting, workspace tests and Clippy with warnings denied
passed again after adding the icon. Distribution signing/notarization remain
release gates.


## FLAC/MP3 Collector originals — 2026-10-19

Real TLS tests import actual high-resolution FLAC and MP3 purchases into an
isolated Collector, stream/decode/seek both, retain independent downloads and
artwork/text, remove credentials/stop the server and reopen/decode offline.
The separately invoked real-device test opens a fresh Player for each format:
FLAC callback peak 0.088371746 and MP3 0.08895567, each beyond 0.0213125 seconds.
Workspace tests, strict all-target Clippy and formatting pass. Collector's dated
HL-02 validation records the full fixture/client matrix and final reruns.
Actual desktop GUI acceptance passed after the user unlocked this Mac. A fresh
profile and real TLS fixture served 30-second 96-kHz stereo 24-bit FLAC and
44.1-kHz stereo VBR MP3 originals with a 256×256 cover. The actual Slint app paired,
browsed album/artwork, played/sought both, advanced tracks, queued readable
artist/title labels and downloaded both originals. Removing the connection stayed
responsive and retained downloads. After quitting, stopping the server and
reopening without server arguments, playback started paused; both downloads,
artwork and offline search remained available, and FLAC/MP3 played and sought.
Collector's dated HL-02 record includes the actual offline screenshot.

The initial GUI run exposed source IDs in captions and a main-thread Keychain
removal stall after replacing the enrolled executable. Captions now show artist
text and credential removal runs on a worker with pending state. The final repeat
kept executable identity unchanged and completed removal/reopen successfully.
After those repairs, full workspace tests passed (53 passed, 2 ignored), strict
all-target Clippy, formatting and the app build passed. No subjective-fidelity,
gapless-output or broader HL-04 device/LAN qualification is inferred.


## HL-02 pinned-revision publication — 2026-10-19

The user authorized committing/pushing this delivery and removing local patches.
Cargo manifests pin junk-libs `2c8eaf8fd5a7f3e5f4873d3463b8008039e9af98` and
Collector `201085448cbd727bd04f60275e47431c2c7dd3bd`. All sibling packages resolve
from those exact Git sources. Unrelated staged alpha packaging/release changes
are preserved locally and excluded from this delivery commit. Publication checks passed against an isolated export of the candidate
source and the working checkout, using Rust 1.95.0:

- Isolated HL-02 workspace: 51 passed, 2 existing opt-in tests ignored; strict
  all-target Clippy, formatting and the normal desktop build passed.
- Working checkout with preserved staged release changes: 53 passed, 2 ignored;
  strict all-target Clippy and formatting passed. The additional two tests belong
  to that separate release work and are excluded from the HL-02 commit.
- The real CoreAudio callback test was invoked separately from the isolated
  source and passed for both formats: FLAC peak 0.088371746, MP3 0.08895567,
  each beyond 0.0213125 seconds.
- Cargo metadata confirms all 16 Collector/junk-libs packages resolve from the
  two exact Git revisions above. No sibling path or Cargo patch remains.

The first isolated build caught three `include_bytes!` references to media in a
sibling Collector checkout. Those generated fixtures now live under
`crates/amp-collector/tests/fixtures/digital-audio`, making both the tests and
acceptance example self-contained. The isolated gates passed after this repair.
Every candidate tracked file was compared with the isolated checked source;
alpha packaging and unrelated release changes remain outside this commit.

## Automatic private Collector playback — 2026-10-22

The full workspace tests and `cargo clippy --workspace --all-targets -- -D warnings`
pass against Collector Git pin `14d56ab4cceaf82c09086d56d987484bab46d618`, with
`cargo fmt --all -- --check` clean. No local path patches are checked in or needed.
The real-server integration fixture disables media on the enrolled control URL
and starts a separate automatically trusted private media listener. Actual FLAC
decoding, seeking, explicit downloads, revocation, retained cache and offline
playback pass. Existing server/account/cache keys are retained. Hardware-audio,
new-build NAS/work-Mac deployment and managed-VPN qualification remain distinct;
these tests do not establish zero WAN traffic for arbitrary private OS routes.
See [the connection contract](collector-connection.md#automatic-private-playback--2026-10-22).

## Beta release preparation — 2026-10-23

Tested the working tree based on `fb70a4a` plus the beta release preparation
changes, using pinned Rust 1.95.0 on arm64 macOS 26.6.2. Workspace version is
`0.1.0-beta.1`; Cargo.lock changes only the five workspace package versions.
This is local packaging evidence, not separate-Mac or minimum-OS acceptance.

- `cargo fmt --all --check`, strict locked all-target Clippy and locked workspace
  tests passed: 53 tests passed, two opt-in tests skipped (hardware output and
  live MusicBrainz). No hardware audio test was invoked for this preparation.
- Three Python release-version tests passed, covering alpha/beta metadata,
  increasing build numbers across channels/sequences and rejected malformed or
  unsupported versions. ShellCheck, shell syntax and updated Markdown links
  passed; the workflow YAML parsed and includes the beta tag trigger.
- An actual `RELEASE_TAG=v0.1.0-beta.2` package invocation rejected the mismatch
  with Cargo's beta.1 version before building.
- `RELEASE_TAG=v0.1.0-beta.1 ./scripts/package-macos.sh` built the optimized app,
  generated runtime/native notices, ad-hoc signed the bundle and created the ZIP
  and SHA-256 file under `target/dist`.
- `./scripts/verify-macos-package.sh target/dist/trash-amp-0.1.0-beta.1-macos-arm64.zip`
  passed: checksum, extracted executable, arm64 architecture, plist, signature,
  dependency notices, system-only dynamic dependencies and `--help`. Verified
  metadata is short version `0.1.0`, build `2.1`, full version `0.1.0-beta.1` and
  minimum OS `15.0`; the archive filename must match the embedded full version.

At this initial preparation stage no tag, remote draft or published release had
been created. Developer ID signing, notarization, CI on the final commit,
upgrade/Keychain checks and the remaining native beta acceptance gates were open;
later records below document progress against [the checklist](release-gates.md).

## Shared team signing credentials — 2026-10-23

Inspected Print Junk's local release workflow and its successful `v0.5.0` release
run to identify the existing signing/notarization setup. Its six GitHub secret
names describe general Apple Developer team credentials, not app-specific
dependencies. Trash Amp uses the same names and its own bundle ID; no Print Junk
repository, executable or entitlement file is required by its build.

Local verification used the working tree based on `fb70a4a` plus beta preparation
and signing changes, on arm64 macOS 26.6.2:

- The existing Developer ID Application identity for team `6DNZ4UX59T` signed an
  isolated copy of the optimized app. Both executable and bundle signing passed;
  the signature reports Hardened Runtime, a secure timestamp, the Developer ID
  authority chain and the expected team.
- The signed binary's `--help` passed. Its actual Slint/Skia demo rendered a
  980×680 snapshot and exited using an isolated profile. The snapshot was visually
  inspected. No JIT or unsigned executable-memory entitlement was needed for
  that smoke test. Audio and upgrade/Keychain acceptance were not tested here.
- Eleven Python release tests passed: three version-policy tests and eight
  signing tests. Stubbed Apple tools cover Accepted versus Invalid status,
  timeout, Gatekeeper rejection, ad-hoc rejection before upload, required
  credentials for tags, generic runner-keychain configuration, certificate-file
  cleanup and certificate/notarization team mismatch.
- Modified shell scripts passed ShellCheck and shell syntax checks; workflow
  YAML parsed with the signed manual-run option. Development ZIP packaging and
  extracted-package verification passed; `--signed` verification correctly
  rejected that ad-hoc ZIP.

At initial implementation Trash Amp had no configured GitHub secrets. Existing repository-secret
values cannot be retrieved through GitHub's API, so they have not been copied.
The GitHub owner is a personal account: configure copies of the same team
credentials in each repository, or use organization secrets if the repositories
later move to an organization. Setup is in [the release guide](macos-release.md).
At that point Apple notarization/stapling and a signed Actions run remained pending;
stubbed tests and successful Developer ID signing do not establish acceptance by
Apple's notary service. No release was published.

## Signed Actions candidate — 2026-10-23

All six required repository secret names were confirmed present. Candidate
`f92294ef9f3327e38afe14ac34ff079684eb9030` was pushed to
`codex/beta-release-pipeline` for a manually dispatched signed build, with the
changes reviewable in [draft PR #1](https://github.com/AberrantWolf/trash-amp/pull/1).
The workflow retains its registered `macos-alpha.yml` filename so signed branch
validation can run before merge; its display name is macOS prerelease and it
supports both alpha and beta tags.

[Signed run 39880984729](https://github.com/AberrantWolf/trash-amp/actions/runs/39880984729)
imported one signing identity and passed the identity/notarization-team match.
Apple then rejected credential validation with HTTP 403: a required agreement
was missing or expired. Cleanup of the temporary runner keychain succeeded.
The Account Holder reported accepting the pending agreement. Both the immediate
retry and a third attempt after a short propagation wait returned the same
account-level rejection; further validation awaits restored notary-service
access. Certificate import does not establish
that the notarization password is valid or that Apple has accepted the app.

[Normal PR run 39881180485](https://github.com/AberrantWolf/trash-amp/actions/runs/39881180485)
passed on the same candidate: formatting, 11 release-tooling tests, strict
all-target Clippy, 53 Rust tests (two existing opt-in tests skipped), optimized
development packaging and extracted-ZIP verification. This run deliberately
used ad-hoc signing, so its success does not qualify notarization.

After the Account Holder confirmed the correct team, active membership and no
remaining agreements, attempt 4 of the signed run validated the notarization
credentials successfully. It then exposed a runner-only signing failure: the
imported temporary keychain was absent from the user search list, which
`codesign` still consults with `--keychain`. Import now preserves and extends that
list; cleanup restores it. A regression test covers both adding the keychain and
restoring paths containing spaces, bringing release-tooling coverage to 12 tests.
Candidate `442187d735c13d16a0b2c2b4cd53360e741ab57c` was validated in
[signed run 39884812620](https://github.com/AberrantWolf/trash-amp/actions/runs/39884812620).
Developer ID signing, Hardened Runtime and timestamps passed. Apple accepted
submission `b50b781c-d396-4f5b-aaa0-0b22cbdbe902`; its result was downloaded and
confirmed as `Accepted`. Stapling then failed with error 68 after Apple's
CloudKit ticket-delivery connection was lost. Bounded retries for transient
ticket-service errors and retention of the signed submission ZIP were added,
with regression tests for recovery, exhaustion and invalid-ticket rejection.
All 15 release-tooling tests passed locally before the final signed ZIP validation
recorded below.

## Completed signed candidate — 2026-10-23

[Signed Actions run 39885698740](https://github.com/AberrantWolf/trash-amp/actions/runs/39885698740)
passed for candidate `554e77a64beb2fa1433354674a5dbafa210add19`, using the configured
team credentials on the Apple Silicon macOS 15 runner. Formatting, strict
all-target Clippy, 53 Rust tests (two opt-in tests skipped), all 15 release-tooling
tests, optimized packaging and extracted-package verification passed. Temporary
signing-keychain cleanup also passed.

Apple accepted submission `a835f3f1-ce2b-4043-bdb7-d6f8a1c95b7a`; the retained
notary log reports `Ready for distribution`, status code 0 and `issues: null`.
Developer ID/Hardened Runtime/timestamp checks, stapling, ticket validation and
Gatekeeper assessment passed on both the staged bundle and extracted final ZIP.
Gatekeeper reported `source=Notarized Developer ID`. The successful run did not
need to exercise the transient ticket-delivery retry.

Downloaded the exact `macos-arm64-prerelease` Actions artifact to
`target/dist/signed-ci` and ran the signed verifier locally on arm64 macOS 26.6.2.
Checksum, metadata, architecture, notices, signature, stapled ticket, Gatekeeper,
system-only dynamic dependencies and `--help` all passed. The distribution ZIP
SHA-256 is `18d23eb51ab273a1e570602979caddf528873797b66614577e32a3a14659cd66`.
The diagnostic artifact also retains the exact signed submission ZIP, Apple's
submission result and full notary log for recovery/audit.

The prior agreement restriction cleared without changing credentials after the
Account Holder accepted the pending agreement and confirmed membership/team
state. First launch on another Mac, minimum-OS and upgrade/Keychain/native audio
acceptance remain separate beta gates. No beta tag or published release was
created; the changes remain in draft PR #1.

## Output-device recovery — 2026-10-23

[PR #1](https://github.com/AberrantWolf/trash-amp/pull/1) was squash-merged as
`fef50fc4a91d802cee51267a8a236a6e93a8b04f`. Recovery implementation
`33b27840c2faba4751b6b219f5254be4ffb837f3` was tested on arm64 macOS 26.6.2,
using pinned Rust 1.95.0. No release tag or published release was created.

- `cargo test --workspace --locked` passed: 63 tests, with the two existing
  opt-in tests skipped. The audio crate has 15 passing tests, including ten new
  recovery cases that use the production worker, decoder and render callback
  with a controllable device backend. They cover failed/stalled streams,
  default-output changes, callback position versus decoder read-ahead, retained
  queue and exact resumed PCM, changed sample rates, seeks and queue edits with
  no output, pending Play/Pause, clearing stale audio, retired callbacks, retry
  backoff, and responsive transport/drop during blocked device setup.
- Strict locked all-target Clippy, formatting and whitespace checks passed.
- The actual CoreAudio silence probe advanced to 1.024 seconds with zero
  callback underruns. The separately invoked real-server hardware test passed
  for both FLAC and MP3: peaks 0.088371746 and 0.08895567, each beyond 0.021333
  seconds. These establish callback output, not captured fidelity or gapless
  qualification.
- Both invocations of that optional hardware test printed a background
  `Endpoint cannot be closed` panic during fixture shutdown, although the test
  returned success after both playback assertions. A backtrace identifies the
  pinned Collector server's private WebTransport accept loop at
  `local_playback.rs:184` calling wtransport's `Endpoint::accept`. This is a
  fixture/server teardown limitation; it is recorded rather than treated as a
  clean server-shutdown result. The ordinary workspace integration tests passed.

Device loss or a default-output/configuration change pauses playback, resets the
level meter, retains the last callback position and queue, and reopens the
system-default output on the audio worker. Reconnection stays paused unless an
explicit Play request was made while disconnected. Failed opens retry once per
second; callbacks stalled for three seconds also trigger recovery. No source
decoding or device opening is performed on the UI thread.

Physical USB/Bluetooth removal/reconnect, repeated output changes, differing
hardware sample rates and sleep/wake still require native-app qualification in
the extracted final package. The corresponding beta gate remains unchecked.

## Published beta and automatic publication — 2026-10-23

PR #2 merged as `28c2c9062dcfd94e2fab708ba96e6538241aa626`. Main CI passed, and
`v0.1.0-beta.1` points to that exact commit.
[Tagged run 39899784445](https://github.com/AberrantWolf/trash-amp/actions/runs/39899784445)
passed on attempt 2: 63 Rust tests, 15 release-tooling tests, formatting, strict
Clippy, signing, notarization, stapling, package verification and release asset
checksums. Attempt 1 failed only when extracted-ticket validation lost its
CloudKit connection (error 68); initial notarization and stapling had passed.

Apple accepted the final submission `843001b7-ab83-4d61-baa3-ae0e5e126861` with
status code 0 and no issues. The downloaded ZIP passed local signed verification,
including the stapled ticket and Gatekeeper's `Notarized Developer ID` result.
Its SHA-256 is `b0a3fa66b97ded77443f69465a75a043997fa1020d9ebd9dd3a0872a0bdd4145`.
The workflow initially created a draft; beta.1 was then manually published to
make its downloads visible. This exposed the missing automated publication step.

Automation correction `1aa5388e8a0841dfe2d8c80586e3dc39fc30f4ce` was checked on
arm64 macOS 26.6.2 with 28 passing release-tooling tests, ShellCheck, shell syntax,
workflow YAML parsing and whitespace checks. Publication tests cover new
prereleases, interrupted drafts, failed uploads/creation, service failures,
corrupt artifacts and reruns of published releases. Stubbed CLI tests do not
claim a new live release publication. The real published-beta rerun path was
also exercised: it returned success without changing the notes or asset hashes.

Ticket tests now cover transient validation failures, retry exhaustion and
invalid-ticket rejection, including the validation-only path used for extracted
packages. The updated signed verifier passed against the exact downloaded beta.1
ZIP without modifying or replacing that archive. Future tag workflows publish
automatically after all build and package checks; existing published tags and
their assets are retained.
