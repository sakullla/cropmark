#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <dispatch/dispatch.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// ADR-17:SCK 回调等待上限。超时返回可读错误,不永久挂起。
#define CROPMARK_SCK_WAIT_SECONDS 10

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

// 阶段日志与 SCK 等待都受 CROPMARK_CAPTURE_TIMING 门控(ADR-17):
// 日志足以区分"未进壳/权限挂起/SCK 回调不触发"。
static bool cropmark_timing_enabled(void) {
  return getenv("CROPMARK_CAPTURE_TIMING") != NULL;
}

static dispatch_time_t cropmark_wait_deadline(void) {
  return dispatch_time(DISPATCH_TIME_NOW, (int64_t)CROPMARK_SCK_WAIT_SECONDS * NSEC_PER_SEC);
}

// 超时放弃后置位:迟到的 SCK 回调不再保留像素/不再信号空等信号量。
static atomic_bool g_sck_wait_abandoned = false;

// 未授权后同一进程内不再走 getShareableContent，避免热键连按反复弹 TCC。
static atomic_int g_sck_content_denied = 0;

// 权限请求每进程只发起一次(CGRequest 的弹窗系统也只在首次出现)。
static atomic_int g_sck_permission_requested = 0;

static void cropmark_mark_shareable_content_denied(void) {
  atomic_store(&g_sck_content_denied, 1);
}

static bool cropmark_shareable_content_denied(void) {
  return atomic_load(&g_sck_content_denied) != 0;
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
    cropmark_mark_shareable_content_denied();
    return 1;
  }
  return 2;
}

// 权限请求的限界派发:CGRequestScreenCaptureAccess 自身立即返回(弹窗由系统进程
// 展示),这里只对"主队列何时执行请求块"设上限,避免主线程被占用时永久挂起。
static bool cropmark_request_permission_bounded(void) {
  if (atomic_exchange(&g_sck_permission_requested, 1) != 0) {
    return false;
  }
  void (^request)(void) = ^{
    (void)CGRequestScreenCaptureAccess();
  };
  if ([NSThread isMainThread]) {
    request();
    return true;
  }
  dispatch_semaphore_t sema = dispatch_semaphore_create(0);
  dispatch_async(dispatch_get_main_queue(), ^{
    request();
    dispatch_semaphore_signal(sema);
  });
  if (dispatch_semaphore_wait(sema, cropmark_wait_deadline()) != 0) {
    if (cropmark_timing_enabled()) {
      fprintf(stderr, "Cropmark macos sck: permission request dispatch timeout after %ds\n",
              CROPMARK_SCK_WAIT_SECONDS);
    }
    return false;
  }
  return true;
}

// ScreenCaptureKit 的 getShareableContent 在 TCC 未真正绑定时每次都会弹系统授权。
// 先用 CGPreflight 判断；未授权时每进程只请求一次，避免热键连弹。
static int32_t cropmark_sck_ensure_permission(void) {
  if (cropmark_shareable_content_denied()) {
    return 1;
  }
  if (CGPreflightScreenCaptureAccess()) {
    return 0;
  }
  if (cropmark_timing_enabled()) {
    fprintf(stderr, "Cropmark macos sck: permission preflight denied, requesting access\n");
  }
  (void)cropmark_request_permission_bounded();
  if (CGPreflightScreenCaptureAccess()) {
    return 0;
  }
  if (cropmark_timing_enabled()) {
    fprintf(stderr, "Cropmark macos sck: permission still denied after request\n");
  }
  cropmark_mark_shareable_content_denied();
  return 1;
}

// 权限状态(R23):0 已授权;1 未授权且本进程尚未请求(首次会弹系统授权);
// 2 未授权且已请求过(不会再弹窗,需到系统设置开启后重启)。
int32_t cropmark_sck_permission_state(void) {
  if (CGPreflightScreenCaptureAccess()) {
    return 0;
  }
  return atomic_load(&g_sck_permission_requested) != 0 ? 2 : 1;
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

// 获取可共享内容:0 成功;1 超时(回调未在限界内触发);2 失败(权限或其它错误)。
static int32_t cropmark_content(SCShareableContent **outContent, NSError **errorOut) {
  if (outContent) {
    *outContent = nil;
  }
  if (errorOut) {
    *errorOut = nil;
  }
  if (cropmark_shareable_content_denied()) {
    if (errorOut) {
      *errorOut = [NSError errorWithDomain:@"CropmarkScreenCapture"
                                      code:1
                                  userInfo:@{NSLocalizedDescriptionKey : @"not authorized"}];
    }
    return 2;
  }
  dispatch_semaphore_t sema = dispatch_semaphore_create(0);
  __block SCShareableContent *content = nil;
  __block NSError *contentError = nil;
  atomic_store(&g_sck_wait_abandoned, false);
  [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *c, NSError *e) {
    if (atomic_load(&g_sck_wait_abandoned)) {
      return;
    }
    content = c;
    contentError = e;
    dispatch_semaphore_signal(sema);
  }];
  if (dispatch_semaphore_wait(sema, cropmark_wait_deadline()) != 0) {
    atomic_store(&g_sck_wait_abandoned, true);
    if (cropmark_timing_enabled()) {
      fprintf(stderr, "Cropmark macos sck: getShareableContent timeout after %ds\n",
              CROPMARK_SCK_WAIT_SECONDS);
    }
    return 1;
  }
  if (!content) {
    (void)cropmark_classify_error(contentError);
    if (errorOut) {
      *errorOut = contentError;
    }
    return 2;
  }
  if (outContent) {
    *outContent = content;
  }
  if (errorOut) {
    *errorOut = contentError;
  }
  return 0;
}

static bool cropmark_capture_filter(SCContentFilter *filter, SCStreamConfiguration *config, CropmarkSckResult *out) {
  dispatch_semaphore_t sema = dispatch_semaphore_create(0);
  __block CGImageRef captured = NULL;
  __block NSError *capError = nil;
  atomic_store(&g_sck_wait_abandoned, false);
  [SCScreenshotManager captureImageWithFilter:filter
                                configuration:config
                            completionHandler:^(CGImageRef image, NSError *error) {
                              if (atomic_load(&g_sck_wait_abandoned)) {
                                return;
                              }
                              if (image) {
                                captured = CGImageRetain(image);
                              }
                              capError = error;
                              dispatch_semaphore_signal(sema);
                            }];
  if (dispatch_semaphore_wait(sema, cropmark_wait_deadline()) != 0) {
    atomic_store(&g_sck_wait_abandoned, true);
    if (cropmark_timing_enabled()) {
      fprintf(stderr, "Cropmark macos sck: captureImage timeout after %ds\n",
              CROPMARK_SCK_WAIT_SECONDS);
    }
    cropmark_set_error(out, 6, "ScreenCaptureKit 截取超时。");
    return false;
  }
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

// SCDisplay.width 在部分系统上是点不是像素。用 NSScreen.frame × backingScale
// 对齐 Retina backing,避免 1x 抓屏画到 2x 视图上发糊、选区也对不齐。
static CGFloat cropmark_backing_scale_for_display(SCDisplay *display) {
  CGFloat width = CGRectGetWidth(display.frame);
  CGFloat height = CGRectGetHeight(display.frame);
  for (NSScreen *screen in [NSScreen screens]) {
    NSRect frame = screen.frame;
    if (llround(NSWidth(frame)) == llround(width) && llround(NSHeight(frame)) == llround(height)) {
      return screen.backingScaleFactor > 0 ? screen.backingScaleFactor : 1.0;
    }
  }
  NSScreen *main = [NSScreen mainScreen];
  return main && main.backingScaleFactor > 0 ? main.backingScaleFactor : 1.0;
}

static void cropmark_configure_display_capture(SCStreamConfiguration *config, SCDisplay *display) {
  CGFloat scale = cropmark_backing_scale_for_display(display);
  size_t width = (size_t)llround(CGRectGetWidth(display.frame) * scale);
  size_t height = (size_t)llround(CGRectGetHeight(display.frame) * scale);
  config.width = width > 0 ? width : 1;
  config.height = height > 0 ? height : 1;
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
  SCShareableContent *content = nil;
  int32_t contentStatus = cropmark_content(&content, &contentError);
  if (contentStatus == 1) {
    cropmark_set_error(out, 5, "ScreenCaptureKit 获取可共享内容超时。");
    return -1;
  }
  if (contentStatus != 0) {
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
  cropmark_configure_display_capture(config, chosen);
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
  SCShareableContent *content = nil;
  int32_t contentStatus = cropmark_content(&content, &contentError);
  if (contentStatus == 1) {
    cropmark_set_error(out, 5, "ScreenCaptureKit 获取可共享内容超时。");
    return -1;
  }
  if (contentStatus != 0) {
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
  SCShareableContent *content = nil;
  int32_t contentStatus = cropmark_content(&content, &contentError);
  if (contentStatus == 1) {
    return 3;
  }
  if (contentStatus != 0) {
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
