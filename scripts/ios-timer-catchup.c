// Exercise the same CoreFoundation rescheduling path as winit's iOS waker.
// Built and compared with the original and patched intervals by test-ios-timer.py.
#include <CoreFoundation/CoreFoundation.h>
#include <float.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static double fired;

static void callout(CFRunLoopTimerRef timer, void *info) {
    (void)timer;
    (void)info;
    fired = CFAbsoluteTimeGetCurrent();
    CFRunLoopStop(CFRunLoopGetCurrent());
}

int main(int argc, char **argv) {
    if (argc != 3) return 2;
    double away = strtod(argv[1], NULL);
    bool patched = strtol(argv[2], NULL, 10) != 0;
    CFRunLoopTimerRef timer = CFRunLoopTimerCreate(
        NULL, DBL_MAX, patched ? DBL_MAX : 1e-7, 0, 0, callout, NULL);
    CFRunLoopAddTimer(CFRunLoopGetCurrent(), timer, kCFRunLoopCommonModes);
    CFRunLoopTimerSetNextFireDate(timer, CFAbsoluteTimeGetCurrent() + 0.016);
    struct timespec pause = {(time_t)away, (long)((away - (time_t)away) * 1e9)};
    nanosleep(&pause, NULL);
    CFRunLoopRunInMode(kCFRunLoopDefaultMode, 5, false);
    double catchup_ms = (CFAbsoluteTimeGetCurrent() - fired) * 1000;
    bool valid = CFRunLoopTimerIsValid(timer);
    bool parked = CFRunLoopTimerGetNextFireDate(timer) > CFAbsoluteTimeGetCurrent() + 31536000;
    printf("{\"patched\":%s,\"freeze_seconds\":%.3f,\"catchup_ms\":%.3f,"
           "\"valid\":%s,\"parked\":%s}\n",
           patched ? "true" : "false", away, catchup_ms,
           valid ? "true" : "false", parked ? "true" : "false");
    CFRunLoopTimerInvalidate(timer);
    CFRelease(timer);
    return fired == 0 || !valid || (patched && !parked);
}
