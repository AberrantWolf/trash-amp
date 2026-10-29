# iOS development

The `ios` branch starts an iPhone/iPad host over the existing Rust and Slint UI.
This is a development bootstrap, not an iOS release. Collector streaming, managed downloads and the read-only Device Music Library
are available sources. See [device music](device-music-library.md) for access,
architecture and remaining physical-device qualification. External folder access is not
implemented, so this host hides the folder picker.

## Code boundaries

- `amp-ui` owns the shared `.slint` files, presentation models, navigation and
  catalog presentation wiring. Its `AppConfig` accepts playback and catalog handles,
  storage and native capabilities from a launcher. It contains no OS selection logic.
- `trash-amp` remains the desktop executable. Its launcher supplies the desktop
  data directory, `rfd` folder picker and native media-control window handle.
- `trash-amp-ios` supplies Application Support storage and Souvlaki media controls.
  `amp-audio` owns `AVAudioSession` configuration and activation on the audio
  worker, alongside the device stream. Native metadata updates
  use Apple's main dispatch queue, independently of Slint timers.
- Domain values, catalog search, decoding, Collector transport and persistence
  remain in their existing crates.

`amp-app` now owns playback orchestration outside Slint: the sequence, explicit
queue, transport, engine events and Now Playing publication. Native callbacks
send commands directly to this service. `amp-media` coalesces native updates and
keeps media-control objects on their installation thread. See
[application service](application-service.md) for ownership and validation.

The catalog worker now accepts scan and metadata completion and persists results
without a UI tick. Collector completion chains and selection commits also run
off the presentation loop. Navigation and provider view projection remain in
Slint. A native background execution policy and hardware validation are still
required; this does not establish verified iOS background playback by itself.

The shared layout accounts for window safe-area insets and the virtual keyboard.
Settings and Lists scroll views permit drag panning. These changes use geometry
and capabilities rather than OS names. Existing hover/double-click interactions
still need a touch and accessibility pass.

## Paused playback and battery

Pause stops and drops the CPAL output stream, then deactivates the iOS audio
session with `NotifyOthersOnDeactivation`. Dropping also removes CPAL's native
interruption observers, which could otherwise reactivate a paused session when
another app's interruption ends. Resume activates the session and opens a fresh
stream using the current route and sample rate. The audio worker also releases
the session after queue completion, an output failure or shutdown. Session
operations are serialized with stream operations, so a rapid pause/play cannot
deactivate a newly resumed stream.

The iOS decoder retains up to three seconds of unplayed PCM, the queue, the exact
audible position and its decoder/reader state. A bounded worker-side mirror keeps
the last device-buffer's worth of produced frames; pause trims already played
frames after the device stops. The real-time callback still uses only its ring
and atomics. At 44.1/48 kHz the paused frame cache uses roughly 4–5 MB, including
per-frame position metadata, plus the decoder's existing bounded buffers.

The worker sleeps until a transport/queue command or shutdown, instead of polling
every three milliseconds. Collector's reader uses a current-thread runtime and
fetches on demand: retaining it does not issue network requests while paused.
Resume preloads the saved PCM before starting the new device, then continues
decoding from the end of read-ahead while the callback plays that buffer. No
reader reopen or seek is needed for an unchanged sample rate. A seek, sequence
edit or sample-rate change discards incompatible PCM and rebuilds at the audible
position. A slow connection can still exhaust the available buffer; three seconds
is a maximum runway, not a guarantee of uninterrupted resume.

Desktop streams retain their buffered audio while paused, with one-second checks
for asynchronous device errors. Devices without hardware pause retain software
pause. Default-device polling and the callback watchdog are disabled for a
deliberately paused stream; resume resets the watchdog. The inactive session
allows iOS to suspend the app when it has no other background work. UI and
application timers do not need to keep the process alive for paused playback or
lock-screen resume.

Regression tests cover exact samples across a pause longer than the watchdog,
paused seeks/queue edits, repeated buffer rollover, delayed network reads during
resume, sample-rate changes, natural queue completion and output recovery. Physical
iPhone validation is still required: play, pause, lock for several minutes, and
compare CPU/audio activity in Instruments and battery use with the prior build.
Check lock-screen/Bluetooth resume, rapid pause/play, completion, another app's
audio, and headphone/Bluetooth route changes during a pause. Build checks and
fake-device tests do not measure device battery savings.

On 2026-10-28, optimized Release build 2.5 passed signature verification and was
installed and launched on the user's iPhone 16 Pro without an attached debugger.
The user reported an initial improvement. This is preliminary hardware feedback;
longer battery measurements and the remaining device checks are still open.

## Appearance

Settings → General offers Light and Dark appearance plus an independent
OLED black backgrounds toggle. Choices persist across restarts. OLED applies
only in Dark mode, replacing the shared canvas, panel, dialog and transport
backgrounds with pure black; focus, selection and interactive states remain
visible. Existing profiles default to Dark with OLED off.

The shared UI pins Slint's Fluent widget style on both iOS and desktop so
buttons, menus, text fields and scrollbars have the same appearance. The shared
palette uses warm charcoal, soft ivory and restrained amber from
the headphones icon. Standard menus and text fields follow the selected color
scheme; transport buttons, sliders and checkboxes use the shared accent.
For reproducible visual checks, use `--color-mode light` or `--color-mode dark`
and optionally `--oled` with the existing `--snapshot` and `--size` arguments.
These command-line overrides do not save preferences.

## Local builds

The repository includes the patched winit source and Cargo selects it
automatically. Normal build commands use the fix; no manual dependency patching
or changes to Cargo's global cache are needed.

Use the pinned Rust toolchain and a full Xcode installation with iOS SDKs:

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
cargo check -p trash-amp-ios --target aarch64-apple-ios-sim
./scripts/bundle-ios.sh --simulator
```

The simulator bundle is written to
`target/aarch64-apple-ios-sim/debug/Trash Amp.app`. It is signed ad hoc and needs no
Apple account. With an available simulator:

```sh
xcrun simctl list devices available
xcrun simctl boot DEVICE_UDID
xcrun simctl bootstatus DEVICE_UDID -b
xcrun simctl install DEVICE_UDID 'target/aarch64-apple-ios-sim/debug/Trash Amp.app'
xcrun simctl launch DEVICE_UDID org.aberrantwolf.TrashAmp.iOS --demo
```

Omit `--demo` to connect to Collector through Settings / Servers. A publicly
trusted HTTPS server is the simplest first connection. Importing private trust
material through the iOS document picker remains unfinished.

`--device` builds an unsigned device bundle. `--release` selects an optimized
build. The script targets Apple Silicon simulators and ARM64 devices, with a
provisional iOS 15 deployment minimum. This minimum is not a tested support claim.

For device signing, install XcodeGen, then generate the project:

```sh
cd assets/ios
xcodegen generate
open TrashAmp.xcodeproj
```

Select your development team in Xcode's Signing & Capabilities panel and choose
your connected device. Account login, provisioning, device trust and Developer
Mode may require your interaction. The project invokes the same Cargo build
script; it does not introduce a second UI. Xcode and standalone bundles include
the headphones app icon from `assets/ios/Assets.xcassets`; see the
[icon packaging notes](../assets/ios/README.md). Archive/TestFlight qualification
and release numbering remain future work.

The Rust build phase declares `$(TARGET_BUILD_DIR)/$(EXECUTABLE_PATH)` as an
output. Keep this declaration: the script replaces the executable on every
build, and Xcode must re-sign the bundle afterward. Without the output dependency,
an incremental build can reuse the previous signing step and leave an unsigned
executable inside the app.

Slint 1.18.1 provides `SlintWindowSceneDelegate`. The bundle manifest opts into
its UIKit scene lifecycle, as required for SDK 27 builds on iOS 27. Keep the
manifest and backend version aligned. Simulator and signed device launch are
verified below.

The iOS host supplies Collector credential storage with
`AfterFirstUnlockThisDeviceOnly` accessibility. It upgrades existing items in
place during foreground startup. Credentials remain available for subsequent
streamed tracks while the phone is locked after its first unlock. The shared UI
receives the configured adapter from its launcher.

The playback worker supplies album titles and cached artwork paths to native Now
Playing updates, retaining album context for queued tracks when the browsing
source changes. The iOS host decodes cached artwork once when its path changes and
keeps the native artwork on subsequent progress updates. Missing or invalid files
omit the cover. The host also supplies an explicit playback rate: 1 while playing
and 0 while paused. Text, duration, elapsed time, rate and artwork are published
together in one native dictionary update, so a seek does not first expose metadata
without its progress or artwork. Souvlaki handles remote commands without writing
intermediate metadata on iOS. Clearing playback removes the metadata. This uses
the existing coalesced main-queue dispatcher and does not depend on foreground UI
polling.

## Validation on 2026-10-25

All 97 active workspace tests pass. Seven new concurrency regressions cover
engine advancement between application polls, batched transport commands,
audio-callback exclusion during queue edits, and cancellation during local or
external selection persistence. Another regression verifies that a cleared
Collector selection survives reopening. Workspace and iOS-target Clippy pass
with warnings denied. The 28 release-tooling tests pass.

The simulator and unsigned device bundles build with Slint 1.18.1. The simulator
bundle passes ad-hoc signature verification. Its scene manifest matches the
compiled `SlintWindowSceneDelegate`. Native iOS code supplies the explicit
Now Playing playback rate and Keychain accessibility policy. Third-party notices
regenerate successfully for the upgraded Slint/Skia dependencies. The rebuilt
macOS launcher renders the shared UI at 390 × 844 and exits cleanly.

Xcode first-launch setup completed and updated CoreSimulator to 1174.9.2.
The iOS 27.0 and 27.1 simulator runtimes are installed.
Trash Amp installs and launches on the iPhone 18 Pro simulator running iOS 27.0.
The native screen and accessibility tree show the shared library UI, Settings,
source selection and playback controls. Connection failures show an embedded
modal and block the underlying controls until dismissed. This verifies startup through the scene
lifecycle. No administrator or Apple account authentication was requested.

In Xcode 27.1, open the simulator viewer through **Xcode → Open Developer Tool →
Device Hub**. The former standalone Simulator app is absent from this installation.
Install runtimes in **Xcode → Settings → Components** if `simctl list runtimes`
is empty. The first `simctl` call may run `xcodebuild -runFirstLaunch`; allow the
component installation to finish before retrying it.

Signed development builds now install and launch on a physical iPhone. Repeated
incremental builds pass signature verification. Hardware testing confirms playback
continues while locked, and the user confirms lock-screen scrubbing remains stable
after publishing complete Now Playing updates atomically. Artwork is wired into
native updates; visual hardware confirmation and locked-screen track transitions
still need qualification. Collector enrollment and library loading are verified
below.

The 23 UI tests include repeated Light/Dark selection in both directions, checking
the preference callback receives the selected mode. The 25 application tests and
three media-adapter tests cover artwork arrival, background queue transitions,
retaining queued album context across source changes, seeks retaining metadata,
and bypassing intermediate backend metadata writes on iOS. Application/media and
iOS-target Clippy checks pass with warnings denied.

## Collector connection integration

Simulator Keychain access requires an application identifier and access group.
`amp-ios/build.rs` embeds `assets/ios/Simulator.entitlements` in the executable's
`__TEXT,__entitlements` section, matching Xcode's simulator linker behavior. The
bundle keeps a plain ad-hoc signature. Do not pass the iOS entitlements to macOS
`codesign --entitlements`: macOS rejects those restricted entitlements on an
ad-hoc executable. Device builds use their normal Xcode signing entitlements.

The iOS host enables Reqwest's native TLS backend, which validates HTTPS with
Apple's trust store. The SDK's Rustls native-root loader otherwise treats iOS as
Unix and looks for certificate files instead of using the system trust store.
Private pairing still supplies its explicit trust anchors with built-in roots
disabled. Certificate and hostname verification remain enabled.

The host also opens enrollment approval URLs with `UIApplication`. The shared UI
receives an asynchronous URL-opening capability and displays a modal if opening
fails. It no longer runs the macOS `open` command. Enrollment against
`https://collector.awkwardexplorer.com` completes after browser approval in the
simulator. The enrolled account connects, refreshes its catalog and displays album
artwork. Account approval remains a user action. Regression tests cover invalid URLs,
unreadable trust files, asynchronous connection failures, stale completions,
browser-opening failures, modal input blocking and keyboard dismissal.

## Foreground timer recovery (2026-10-26)

The 2026-10-26 phone watchdog report showed the main thread in
CoreFoundation's repeating-timer catch-up loop during foreground resume.
winit 0.30.13's iOS waker uses a 0.1 µs interval, so a long suspension can
require billions of deadline increments before UIKit can update the scene.
The app's 50 ms UI refresh is not that native repeat interval.

The workspace now selects a vendored winit 0.30.13 with a narrow backport of
[upstream PR #4703](https://github.com/rust-windowing/winit/pull/4703).
The PR is still unmerged and targets winit's newer API; the older idle-timer
fix, #3906, is already in 0.30.13 and does not address this resume stall.
The backport uses a large nonzero repeat interval and manually rearms every
iteration, including unchanged `WaitUntil` deadlines. Provenance, removal
criteria and test commands are in
[the vendor patch notes](../vendor/winit/TRASH_AMP_PATCH.md).

`scripts/test-ios-timer.py` compares the original and patched native CF timers.
Pass `--simulator UDID` to also build and run the real Slint/winit probe with
timed waits, an unchanged expired deadline, idle waits, and process freezes in
`Poll` and `WaitUntil`. Its separate app identifier keeps the user's library
and Collector credentials out of the tests. `--scenario` and `--skip-native`
allow a longer suspension check without repeating the other cases. Logs and
JSON results are written under `target/ios-timer-tests/`.

Validation on 2026-10-26: all 105 active workspace tests pass, as do workspace
and iOS simulator Clippy checks with Rust warnings denied. Simulator and device
bundles build, and the signed device bundle passes signature verification.
Native CF catch-up measured 91.787 ms / 0.009 ms (original / patched) after a
30-second pause, and 557.053 ms / 0.019 ms after 180 seconds. The actual iOS
Slint/winit probe passes idle waits, timed waits, and three consecutive wakeups
for an identical expired deadline. It resumes after 30-second freezes in both
`Poll` and `WaitUntil`; a 180-second `WaitUntil` freeze adds about 29 ms of
resume overhead. Third-party notices regenerate successfully with the vendored
crate. The full simulator app installs and launches.

The signed patched build also installs and launches on the physical iPhone 16
Pro (iOS 27.0.1). With no debugger attached, the same process survives three
minutes in the background and foreground resume, remaining alive 20 seconds
afterward, beyond the 10-second scene-update watchdog allowance. Screenshots
confirm the library UI before and after resume; no crash report was generated
during this check. A second report from 10:53, before installation of the fix,
has the old build UUID and the same `__CFRunLoopDoTimer + 816` watchdog stack.
Overnight physical-device qualification remains outstanding.

## Home/navigation beta.4 install (2026-10-26)

The signed Xcode Debug build for `0.1.0-beta.4`, bundle version `2.4`, installed
and launched on the physical iPhone 16 Pro. Its process remained running after
launch. The in-place installation retained the app data. Home now owns source
and library selection, refresh, Lists and Settings; browsing has no top picker
rows. This verifies signed installation and startup; playback, VoiceOver and
interruption qualification were not repeated for this UI change.

## Remaining work

1. Qualify the independent playback and catalog workers under iOS background
   execution rules. The foreground UI still polls snapshots for visual updates;
   it no longer drives catalog completion or persistence.
2. Reconcile CPAL interruption recovery with player intent and route-loss policy.
   Test screen lock, calls, Siri, headphone removal, Bluetooth and output changes
   on hardware. Define audio-session deactivation when playback ends.
3. Add document selection and security-scoped access for external media and
   private trust files. Keep opaque grants in the host/provider adapter, not in
   shared domain types or UI callbacks that assume a durable filesystem path.
4. Define download suspension/resumption and network-cost policy. Exclude
   reconstructible audio/artwork from backups. Validate Keychain accessibility
   while locked, disk pressure, credential removal and offline playback.
5. Qualify touch input, VoiceOver, text sizing, keyboard avoidance, rotation and
   iPad multitasking. Add mobile-specific tests where they exercise behavior.
6. Establish simulator/device CI, supported OS versions, scene lifecycle validation,
   signed archives, notices, privacy metadata and distribution assets.
