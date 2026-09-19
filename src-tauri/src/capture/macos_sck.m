#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <dispatch/dispatch.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef struct CropmarkSckResult {
  uint8_t *rgba;
  uint32_t width;
  uint32_t height;
  char *error;
  int32_t kind;
} CropmarkSckResult;

typedef struct CropmarkSckMonitor {
  int32_t logical_x;
  int32_t logical_y;
  uint32_t logical_w;
  uint32_t logical_h;
  int32_t physical_x;
  int32_t physical_y;
  uint32_t physical_w;
  uint32_t physical_h;
  double scale;
} CropmarkSckMonitor;

typedef struct CropmarkSckWindow {
  uint32_t window_id;
  uint32_t pid;
  int32_t x;
  int32_t y;
  uint32_t width;
  uint32_t height;
  char title[512];
} CropmarkSckWindow;

static void cropmark_set_error(CropmarkSckResult *out, int32_t kind, const char *msg) {
  out->kind = kind;
  out->error = msg ? strdup(msg) : NULL;
}

static int cropmark_classify_error(NSError *error) {
  if (!error) {
    return 2;
  }
  NSString *domain = error.domain ?: @"";
  NSString *desc = error.localizedDescription ?: @"";
  if ([domain localizedCaseInsensitiveContainsString:@"tcc"] ||
      [desc localizedCaseInsensitiveContainsString:@"tcc"] ||
      [desc localizedCaseInsensitiveContainsString:@"permission"] ||
      [desc localizedCaseInsensitiveContainsString:@"denied"] ||
      [desc localizedCaseInsensitiveContainsString:@"not authorized"] ||
      error.code == -3801 || error.code == -3802) {
    return 1;
  }
  return 2;
}

// ScreenCaptureKit 的 getShareableContent 在 TCC 未真正绑定时每次都会弹系统授权。
// 先用 CGPreflight 判断；未授权时每进程只调用一次 CGRequest，避免热键连弹。
static int32_t cropmark_sck_ensure_permission(void) {
  if (CGPreflightScreenCaptureAccess()) {
    return 0;
  }
  static dispatch_once_t onceToken;
  dispatch_once(&onceToken, ^{
    void (^request)(void) = ^{
      (void)CGRequestScreenCaptureAccess();
    };
    if ([NSThread isMainThread]) {
      request();
    } else {
      dispatch_sync(dispatch_get_main_queue(), request);
    }
  });
  return CGPreflightScreenCaptureAccess() ? 0 : 1;
}

static bool cropmark_cgimage_to_rgba(CGImageRef image, CropmarkSckResult *out) {
  if (!image) {
    cropmark_set_error(out, 3, "截屏缓冲未初始化。");
    return false;
  }
  size_t width = CGImageGetWidth(image);
  size_t height = CGImageGetHeight(image);
  if (width == 0 || height == 0) {
    cropmark_set_error(out, 3, "截屏尺寸为 0。");
    return false;
  }
  size_t bytes = width * height * 4;
  uint8_t *rgba = (uint8_t *)calloc(bytes, 1);
  if (!rgba) {
    cropmark_set_error(out, 3, "截屏缓冲无效。");
    return false;
  }
  CGColorSpaceRef space = CGColorSpaceCreateDeviceRGB();
  CGContextRef ctx = CGBitmapContextCreate(
      rgba, width, height, 8, width * 4, space,
      kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
  CGColorSpaceRelease(space);
  if (!ctx) {
    free(rgba);
    cropmark_set_error(out, 3, "截屏缓冲无效。");
    return false;
  }
  CGContextDrawImage(ctx, CGRectMake(0, 0, width, height), image);
  CGContextRelease(ctx);
  out->rgba = rgba;
  out->width = (uint32_t)width;
  out->height = (uint32_t)height;
  out->kind = 0;
  return true;
}

static SCShareableContent *cropmark_content(NSError **errorOut) {
  dispatch_semaphore_t sema = dispatch_semaphore_create(0);
  __block SCShareableContent *content = nil;
  __block NSError *contentError = nil;
  [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *c, NSError *e) {
    content = c;
    contentError = e;
    dispatch_semaphore_signal(sema);
  }];
  dispatch_semaphore_wait(sema, DISPATCH_TIME_FOREVER);
  if (errorOut) {
    *errorOut = contentError;
  }
  return content;
}

static bool cropmark_capture_filter(SCContentFilter *filter, SCStreamConfiguration *config, CropmarkSckResult *out) {
  dispatch_semaphore_t sema = dispatch_semaphore_create(0);
  __block CGImageRef captured = NULL;
  __block NSError *capError = nil;
  [SCScreenshotManager captureImageWithFilter:filter
                                configuration:config
                            completionHandler:^(CGImageRef image, NSError *error) {
                              if (image) {
                                captured = CGImageRetain(image);
                              }
                              capError = error;
                              dispatch_semaphore_signal(sema);
                            }];
  dispatch_semaphore_wait(sema, DISPATCH_TIME_FOREVER);
  if (capError || !captured) {
    int kind = cropmark_classify_error(capError);
    cropmark_set_error(out, kind,
                       kind == 1 ? "没有屏幕录制权限，未能截取。" : "ScreenCaptureKit 截取失败。");
    if (captured) {
      CGImageRelease(captured);
    }
    return false;
  }
  bool ok = cropmark_cgimage_to_rgba(captured, out);
  CGImageRelease(captured);
  return ok;
}

static NSScreen *cropmark_screen_at(int32_t px, int32_t py) {
  NSPoint point = NSMakePoint(px, py);
  for (NSScreen *screen in [NSScreen screens]) {
    if (NSPointInRect(point, screen.frame)) {
      return screen;
    }
  }
  return [NSScreen mainScreen];
}

void cropmark_sck_free(CropmarkSckResult *out) {
  if (!out) {
    return;
  }
  free(out->rgba);
  free(out->error);
  out->rgba = NULL;
  out->error = NULL;
}

int32_t cropmark_sck_pointer(int32_t *x, int32_t *y) {
  NSPoint loc = [NSEvent mouseLocation];
  if (x) {
    *x = (int32_t)llround(loc.x);
  }
  if (y) {
    *y = (int32_t)llround(loc.y);
  }
  return 0;
}

int32_t cropmark_sck_monitor_at_pointer(CropmarkSckMonitor *out) {
  if (!out) {
    return -1;
  }
  memset(out, 0, sizeof(*out));
  int32_t px = 0;
  int32_t py = 0;
  cropmark_sck_pointer(&px, &py);
  NSScreen *screen = cropmark_screen_at(px, py);
  if (!screen) {
    return -1;
  }
  NSRect frame = screen.frame;
  CGFloat scale = screen.backingScaleFactor > 0 ? screen.backingScaleFactor : 1.0;
  out->logical_x = (int32_t)llround(NSMinX(frame));
  out->logical_y = (int32_t)llround(NSMinY(frame));
  out->logical_w = (uint32_t)llround(NSWidth(frame));
  out->logical_h = (uint32_t)llround(NSHeight(frame));
  out->scale = (double)scale;
  out->physical_x = (int32_t)llround(NSMinX(frame) * scale);
  out->physical_y = (int32_t)llround(NSMinY(frame) * scale);
  out->physical_w = (uint32_t)llround(NSWidth(frame) * scale);
  out->physical_h = (uint32_t)llround(NSHeight(frame) * scale);
  return 0;
}

int32_t cropmark_sck_capture_at_point(int32_t px, int32_t py, CropmarkSckResult *out) {
  memset(out, 0, sizeof(*out));
  if (cropmark_sck_ensure_permission() != 0) {
    cropmark_set_error(out, 1, "没有屏幕录制权限，未能截取。");
    return -1;
  }
  NSError *contentError = nil;
  SCShareableContent *content = cropmark_content(&contentError);
  if (!content) {
    int kind = cropmark_classify_error(contentError);
    cropmark_set_error(out, kind,
                       kind == 1 ? "没有屏幕录制权限，未能截取。"
                                 : "ScreenCaptureKit 无法获取可共享内容。");
    return -1;
  }
  SCDisplay *chosen = nil;
  for (SCDisplay *display in content.displays) {
    CGRect frame = display.frame;
    if (px >= CGRectGetMinX(frame) && px < CGRectGetMaxX(frame) && py >= CGRectGetMinY(frame) &&
        py < CGRectGetMaxY(frame)) {
      chosen = display;
      break;
    }
  }
  if (!chosen) {
    chosen = content.displays.firstObject;
  }
  if (!chosen) {
    cropmark_set_error(out, 4, "没有可用的显示器。");
    return -1;
  }
  pid_t selfPid = [[NSRunningApplication currentApplication] processIdentifier];
  NSMutableArray<SCRunningApplication *> *excluded = [NSMutableArray array];
  for (SCRunningApplication *application in content.applications) {
    if (application.processID == selfPid) {
      [excluded addObject:application];
      break;
    }
  }
  SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:chosen
                                               excludingApplications:excluded
                                                    exceptingWindows:@[]];
  SCStreamConfiguration *config = [SCStreamConfiguration new];
  config.width = (size_t)chosen.width;
  config.height = (size_t)chosen.height;
  config.showsCursor = NO;
  config.capturesAudio = NO;
  if (!cropmark_capture_filter(filter, config, out)) {
    return -1;
  }
  return 0;
}

int32_t cropmark_sck_capture_window(uint32_t window_id, CropmarkSckResult *out) {
  memset(out, 0, sizeof(*out));
  if (cropmark_sck_ensure_permission() != 0) {
    cropmark_set_error(out, 1, "没有屏幕录制权限，未能截取。");
    return -1;
  }
  NSError *contentError = nil;
  SCShareableContent *content = cropmark_content(&contentError);
  if (!content) {
    int kind = cropmark_classify_error(contentError);
    cropmark_set_error(out, kind,
                       kind == 1 ? "没有屏幕录制权限，未能截取。"
                                 : "ScreenCaptureKit 无法获取可共享内容。");
    return -1;
  }
  SCWindow *chosen = nil;
  pid_t selfPid = [[NSRunningApplication currentApplication] processIdentifier];
  for (SCWindow *window in content.windows) {
    if (window.windowID == window_id) {
      if (window.owningApplication.processID == selfPid) {
        cropmark_set_error(out, 2, "窗口截取不含本工具界面。");
        return -1;
      }
      chosen = window;
      break;
    }
  }
  if (!chosen) {
    cropmark_set_error(out, 2, "找不到该窗口。");
    return -1;
  }
  SCContentFilter *filter = [[SCContentFilter alloc] initWithDesktopIndependentWindow:chosen];
  SCStreamConfiguration *config = [SCStreamConfiguration new];
  config.width = (size_t)llround(CGRectGetWidth(chosen.frame) * [NSScreen mainScreen].backingScaleFactor);
  config.height = (size_t)llround(CGRectGetHeight(chosen.frame) * [NSScreen mainScreen].backingScaleFactor);
  config.showsCursor = NO;
  if (!cropmark_capture_filter(filter, config, out)) {
    return -1;
  }
  return 0;
}

int32_t cropmark_sck_list_windows(CropmarkSckWindow *out, int32_t cap, int32_t *count) {
  if (count) {
    *count = 0;
  }
  if (cropmark_sck_ensure_permission() != 0) {
    return 1;
  }
  NSError *contentError = nil;
  SCShareableContent *content = cropmark_content(&contentError);
  if (!content) {
    return cropmark_classify_error(contentError) == 1 ? 1 : 2;
  }
  pid_t selfPid = [[NSRunningApplication currentApplication] processIdentifier];
  int32_t n = 0;
  for (SCWindow *window in content.windows) {
    if (n >= cap) {
      break;
    }
    if (!window.isOnScreen || window.owningApplication.processID == selfPid) {
      continue;
    }
    NSString *title = window.title ?: @"";
    if (title.length == 0) {
      continue;
    }
    NSScreen *screen = cropmark_screen_at((int32_t)llround(CGRectGetMidX(window.frame)),
                                          (int32_t)llround(CGRectGetMidY(window.frame)));
    CGFloat scale = screen && screen.backingScaleFactor > 0 ? screen.backingScaleFactor : 1.0;
    CropmarkSckWindow *slot = &out[n];
    slot->window_id = window.windowID;
    slot->pid = (uint32_t)window.owningApplication.processID;
    slot->x = (int32_t)llround(CGRectGetMinX(window.frame) * scale);
    slot->y = (int32_t)llround(CGRectGetMinY(window.frame) * scale);
    slot->width = (uint32_t)llround(CGRectGetWidth(window.frame) * scale);
    slot->height = (uint32_t)llround(CGRectGetHeight(window.frame) * scale);
    memset(slot->title, 0, sizeof(slot->title));
    const char *utf8 = title.UTF8String;
    if (utf8) {
      strncpy(slot->title, utf8, sizeof(slot->title) - 1);
    }
    n += 1;
  }
  if (count) {
    *count = n;
  }
  return 0;
}
