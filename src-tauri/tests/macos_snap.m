// Exercise the production window enumerator with a deterministic CG window list.
// No screen recording or Accessibility permission is needed.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <assert.h>
#include <unistd.h>

static NSArray<NSDictionary *> *test_windows;

static CFArrayRef test_window_list(CGWindowListOption options, CGWindowID relative_to) {
  assert(options == (kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements));
  assert(relative_to == kCGNullWindowID);
  return (CFArrayRef)CFRetain((__bridge CFArrayRef)test_windows);
}

#define CGWindowListCopyWindowInfo test_window_list
#include "../src/capture/macos_sck.m"
#undef CGWindowListCopyWindowInfo

static NSDictionary *window_info(int32_t pid, int32_t layer, CGRect bounds,
                                 bool visible, double alpha) {
  return @{
    (id)kCGWindowOwnerPID: @(pid),
    (id)kCGWindowLayer: @(layer),
    (id)kCGWindowBounds: CFBridgingRelease(CGRectCreateDictionaryRepresentation(bounds)),
    (id)kCGWindowIsOnscreen: @(visible),
    (id)kCGWindowAlpha: @(alpha),
    (id)kCGWindowName: @"Test window",
  };
}

int main(void) {
  @autoreleasepool {
    [NSApplication sharedApplication];
    CGRect screen = CGRectMake(0, 0, 1920, 1080);
    CGRect target = CGRectMake(100, 100, 600, 400);
    NSMutableArray *windows = [NSMutableArray array];
    // Transparent system surfaces may still advertise alpha=1. They must not
    // win the hit test or exhaust the result capacity before application windows.
    for (int i = 0; i < 8; i++) {
      [windows addObject:window_info(42, kCGDockWindowLevel, screen, true, 1.0)];
    }
    int32_t system_layers[] = {kCGDesktopWindowLevel, kCGMainMenuWindowLevel,
                               kCGStatusWindowLevel, kCGScreenSaverWindowLevel};
    for (size_t i = 0; i < sizeof(system_layers) / sizeof(system_layers[0]); i++) {
      [windows addObject:window_info(42, system_layers[i], screen, true, 1.0)];
    }
    [windows addObject:window_info((int32_t)getpid(), 0, screen, true, 1.0)];
    [windows addObject:window_info(42, 0, screen, false, 1.0)];
    [windows addObject:window_info(42, 0, screen, true, 0.0)];
    [windows addObject:window_info(42, 0, CGRectMake(800, 800, 100, 100), true, 1.0)];
    [windows addObject:window_info(42, 0, CGRectMake(100, 100, 0, 400), true, 1.0)];
    [windows addObject:window_info(101, kCGFloatingWindowLevel, target, true, 1.0)];
    [windows addObject:window_info(102, kCGModalPanelWindowLevel, target, true, 1.0)];
    [windows addObject:window_info(103, kCGNormalWindowLevel, target, true, 1.0)];
    test_windows = windows;

    CropmarkSnapWindow hits[8];
    int32_t count = -1;
    assert(cropmark_snap_windows_at(150, 150, hits, 8, &count) == 0);
    assert(count == 3);
    for (int i = 0; i < count; i++) {
      assert(hits[i].pid == 101 + i);
      assert(hits[i].x == 100 && hits[i].y == 100);
      assert(hits[i].width == 600 && hits[i].height == 400);
    }
    assert(cropmark_snap_windows_at(150, 150, hits, 1, &count) == 0);
    assert(count == 1 && hits[0].pid == 101);
    assert(cropmark_snap_windows_at(1900, 1000, hits, 8, &count) == 0);
    assert(count == 0);
    test_windows = @[];
    assert(cropmark_snap_windows_at(150, 150, hits, 8, &count) == 0);
    assert(count == 0);
    puts("macOS snap window regression tests passed");
  }
  return 0;
}
