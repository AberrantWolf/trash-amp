#!/usr/bin/env python3
"""Compare native CF catch-up and optionally test the real Slint/winit iOS loop.

No app data is used. The simulator probe has its own application identifier.
Run with --simulator UDID for Poll, Wait, timed/reused deadlines and SIGSTOP/CONT.
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import plistlib
import re
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "target/ios-timer-tests"
BUNDLE_ID = "org.aberrantwolf.TrashAmp.TimerProbe"


def run(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True, stderr=subprocess.STDOUT)


def native_comparison(seconds):
    source = (ROOT / "vendor/winit/src/platform_impl/ios/app_state.rs").read_text()
    # Keep this harness tied to the production timer's selected interval.
    assert re.search(r"CFRunLoopTimerCreate\(\s*ptr::null_mut\(\),\s*f64::MAX,\s*f64::MAX,", source)
    executable = OUTPUT / "cf-timer-catchup"
    run("xcrun", "clang", "-O2", "-framework", "CoreFoundation",
        str(ROOT / "scripts/ios-timer-catchup.c"), "-o", str(executable))

    def measure(mode):
        return json.loads(run(str(executable), str(seconds), str(mode)))

    print(f"Comparing native timers after {seconds:g}s without main-loop service", flush=True)
    with concurrent.futures.ThreadPoolExecutor() as pool:
        baseline, patched = pool.map(measure, [0, 1])
    print(json.dumps({"baseline": baseline, "patched": patched}), flush=True)
    assert patched["valid"] and patched["parked"]
    assert patched["catchup_ms"] < 100, "Patched CF timer stalled"
    if seconds >= 10:
        assert baseline["catchup_ms"] > max(10, patched["catchup_ms"] * 4), "Baseline did not reproduce catch-up"
    return {"baseline": baseline, "patched": patched}


def prepare_probe(device):
    print("Building native iOS event-loop probe", flush=True)
    subprocess.run(["cargo", "build", "--locked", "-p", "trash-amp-ios", "--example",
                    "timer_probe", "--target", "aarch64-apple-ios-sim"], cwd=ROOT, check=True)
    app = OUTPUT / "TimerProbe.app"
    app.mkdir(exist_ok=True)
    binary = ROOT / "target/aarch64-apple-ios-sim/debug/examples/timer_probe"
    (app / "timer_probe").write_bytes(binary.read_bytes())
    (app / "timer_probe").chmod(0o755)
    with (ROOT / "assets/ios/Info.plist").open("rb") as f:
        info = plistlib.load(f)
    info.update(CFBundleIdentifier=BUNDLE_ID, CFBundleExecutable="timer_probe",
                CFBundleName="TimerProbe", CFBundleDisplayName="TimerProbe",
                CFBundleVersion="1", CFBundleShortVersionString="0.1.0",
                CFBundleSupportedPlatforms=["iPhoneSimulator"], UIDeviceFamily=[1, 2])
    for key in ("CFBundleIcons", "CFBundleIcons~ipad", "UIBackgroundModes"):
        info.pop(key, None)
    with (app / "Info.plist").open("wb") as f:
        plistlib.dump(info, f)
    run("codesign", "--force", "--sign", "-", str(app))
    run("xcrun", "simctl", "install", device, str(app))


def simulator_case(device, scenario, freeze_seconds=0):
    stdout = OUTPUT / f"{scenario}.stdout"
    stderr = OUTPUT / f"{scenario}.stderr"
    stdout.write_text("")
    stderr.write_text("")
    result = run("xcrun", "simctl", "launch", "--terminate-running-process",
                 "--stdout=" + str(stdout), "--stderr=" + str(stderr), device, BUNDLE_ID, scenario)
    pid = int(result.strip().rsplit(":", 1)[1])
    frozen = False
    actual_freeze = 0
    deadline = time.monotonic() + 15
    try:
        while time.monotonic() < deadline:
            log = stdout.read_text()
            if freeze_seconds and "PROBE_READY" in log:
                # Let about_to_wait return and winit arm its timer before freezing.
                time.sleep(0.1)
                os.kill(pid, signal.SIGSTOP)
                frozen = True
                stopped = time.monotonic()
                print(f"{scenario}: frozen for {freeze_seconds:g}s", flush=True)
                while time.monotonic() - stopped < freeze_seconds:
                    remaining = freeze_seconds - (time.monotonic() - stopped)
                    time.sleep(min(30, max(0, remaining)))
                    if remaining > 30:
                        print(f"{scenario}: freeze in progress", flush=True)
                os.kill(pid, signal.SIGCONT)
                frozen = False
                actual_freeze = time.monotonic() - stopped
                freeze_seconds = 0
                deadline = time.monotonic() + 10
            if "PROBE_PASS " + scenario in log:
                gaps = [float(x) for x in re.findall(r"PROBE_GAP_MS ([0-9.]+)", log)]
                extra_ms = max(gaps, default=0) - actual_freeze * 1000 if actual_freeze else None
                if actual_freeze:
                    assert gaps, "No event-loop gap measured across freeze"
                    assert extra_ms < 150, f"Resume stalled {extra_ms:.1f}ms beyond freeze"
                summary = {"scenario": scenario, "freeze_seconds": actual_freeze,
                           "resume_overhead_ms": extra_ms, "passed": True}
                print(json.dumps(summary), flush=True)
                return summary
            time.sleep(0.02)
        raise AssertionError(f"Probe failed or lost a wakeup: {scenario}\n{log}\n{stderr.read_text()}")
    finally:
        if frozen:
            os.kill(pid, signal.SIGCONT)
        if "PROBE_PASS" not in stdout.read_text():
            # Do not replace a useful test failure if the probe already crashed.
            subprocess.run(["xcrun", "simctl", "terminate", device, BUNDLE_ID],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--freeze-seconds", type=float, default=30)
    parser.add_argument("--simulator", help="Booted simulator UDID; otherwise run the native CF test only")
    parser.add_argument("--skip-native", action="store_true", help="Run simulator cases only")
    parser.add_argument("--scenario", action="append", choices=["fixed-deadline", "wait-until", "wait",
                                                               "poll", "wait-until-freeze"])
    args = parser.parse_args()
    assert 1 <= args.freeze_seconds <= 3600
    OUTPUT.mkdir(parents=True, exist_ok=True)
    assert not args.skip_native or args.simulator
    results = {} if args.skip_native else {"native": native_comparison(args.freeze_seconds)}
    if args.simulator:
        prepare_probe(args.simulator)
        scenarios = args.scenario or ["fixed-deadline", "wait-until", "wait", "poll", "wait-until-freeze"]
        results["simulator"] = [simulator_case(args.simulator, s,
                                args.freeze_seconds if s in ("poll", "wait-until-freeze") else 0)
                                for s in scenarios]
    label = "-".join(args.scenario or ["all" if args.simulator else "native"])
    (OUTPUT / f"results-{args.freeze_seconds:g}s-{label}.json").write_text(json.dumps(results, indent=2) + "\n")
    print("All timer checks passed", flush=True)


if __name__ == "__main__":
    main()
