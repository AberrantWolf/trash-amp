# Trash Amp's winit 0.30.13 backport

This is the published `winit` 0.30.13 crate, under its original Apache-2.0
license. The archive's SHA-256 is
`a6755fa58a9f8350bd1e472d4c3fcc25f824ec358933bba33306d0b63df5978d`;
its upstream Git revision is `e9809ef54b18499bb4f2cac945719ecc2a61061b`.
The workspace's `[patch.crates-io]` selects this copy reproducibly.

Only `src/platform_impl/ios/app_state.rs` differs from the published source:

- Create the waker with a repeat interval of `f64::MAX` instead of 0.1 µs.
  A zero interval would invalidate the timer on its first firing. The large
  nonzero interval keeps it reusable without CoreFoundation iterating through
  billions of expired deadlines after suspension.
- Remove the unchanged-`WaitUntil` optimization. Always rearm a timed wait,
  including an identical deadline which has already fired. `Poll` still rearms
  immediately and `Wait` still parks the timer.

This backports the functional changes in
[upstream PR #4703](https://github.com/rust-windowing/winit/pull/4703), head
`c3f095e54ed19f3a032a887f696b5e7ea95178af`, without its unrelated state-payload
cleanup. That PR was open and unmerged on 2026-10-26. It targets the newer
`winit-uikit` crate; Slint 1.18.1 requires winit's 0.30 API. The latest compatible
release, 0.30.13, and the `v0.30.x` branch still contained the faulty timer.
See [issue #4679](https://github.com/rust-windowing/winit/issues/4679).
The older [PR #3906](https://github.com/rust-windowing/winit/pull/3906) fixes
idle `Wait` behavior, is already present in 0.30.13, and does not fix this stall.

Remove this override when a compatible upstream release includes both changes.
When refreshing the vendor copy, preserve the original license and compare
against the published archive; do not edit Cargo's global registry cache.

## Regression checks

From the repository root on macOS:

```sh
python3 scripts/test-ios-timer.py --freeze-seconds 30
python3 scripts/test-ios-timer.py --freeze-seconds 30 --simulator BOOTED_UDID
```

The native CoreFoundation harness measures the original interval against the
patched interval. The iOS probe uses the actual patched winit through Slint's
backend and checks `Poll`, timed waits, repeated identical expired deadlines,
idle `Wait` with external wakeups, and SIGSTOP/SIGCONT suspension. It uses a
separate simulator app and does not access Trash Amp's saved data.
