# Cropmark

**开源、全离线的跨平台截图套件**:区域 / 窗口 / 全屏截取、标注、贴图、长截图、录屏、离线 OCR,Windows / macOS / Linux 一套搞定。托盘常驻,热键即截,无账号、无云端、不上传任何数据。

![License](https://img.shields.io/badge/License-GPL--3.0-blue)
![Platforms](https://img.shields.io/badge/Platforms-Windows%20%7C%20macOS%20%7C%20Linux-success)
![Offline](https://img.shields.io/badge/OCR-100%25%20offline-orange)
[![Release](https://img.shields.io/github/v/release/sakullla/cropmark)](https://github.com/sakullla/cropmark/releases)

## 为什么选 Cropmark

- **三大桌面平台全覆盖**:同一产品、同一交互,Windows、macOS、Linux 都有安装包。截图 + 贴图 + 长截图 + 录屏 + OCR 这一档功能里,同时支持三平台的选择几乎没有。
- **完全离线**:OCR 模型打进安装包,断网可用;没有账号体系,没有云端同步,没有任何遥测。识别、录制、导出全部在本机完成。
- **开源免费**:GPL-3.0,无广告、无功能订阅、无导出水印。
- **轻量常驻**:Tauri 2 构建,安装包小、内存占用低,平时只待在托盘里。

## 功能一览

### 截图

- 区域、窗口、全屏三种模式,默认热键 `Alt+Shift+A` / `Alt+Shift+W` / `Alt+Shift+S`,均可在设置中修改
- **元素级吸附**:自动识别并吸到窗口或界面控件边缘——Windows 走 UIA、macOS 走 AX、Linux X11 走窗口栈,三层命中自动降级
- 放大镜取色、选区键盘微调、延时截图、截图前自动隐藏自身窗口
- 保留原始物理像素:预览缩放只影响显示,复制与保存始终是原始分辨率无损 PNG
- 多显示器与 DPI 缩放感知的坐标处理

### 长截图

- 滚动拼接长图,支持纵向与横向两个方向
- 显式开始、逐段拼接、中途回退上一段、完成前逐段检视,不依赖不可控的自动滚动

### 录屏

- GIF / WebP / MP4 流式编码,输出即成品
- 录制 HUD:3-2-1 倒计时、空格暂停、Esc 停止,录制框可拖动、就绪态可缩放
- 录制中实时标注;片长与区域对齐所见即所得

### 贴图

- 截图钉在屏幕上置顶显示,支持缩放、透明度调节和鼠标穿透
- 对照资料、临时参考、多屏比较的常用姿势

### 标注

- 矩形、椭圆、箭头、画笔、荧光笔、文字、序号、气泡贴纸、马赛克、模糊、高亮聚焦、橡皮
- 裁剪、旋转,快照式撤销/重做

### OCR 与二维码

- 印刷体**中文 + 英文离线识别**,模型内置安装包,全程不联网
- 按坐标取字:在覆盖层和预览里直接选中识别结果复制
- 本地二维码识别与显式复制

### 导出与美化

- PNG / JPEG / WebP,质量可调,文件名模板化
- 一键美化:留白、圆角、阴影、渐变背景,直接产出适合分享的成品图

### 历史记录

- 截图历史带缩略图,可回看、重新复制、保存或删除
- 删除/清空有 8 秒限时撤销兜底

## 下载安装

从 [Releases](https://github.com/sakullla/cropmark/releases) 获取最新版本:

| 平台 | 格式 | 说明 |
| --- | --- | --- |
| Windows x64 | NSIS `.exe` | 安装包,显示名 Cropmark |
| macOS (Apple Silicon) | `.dmg` | macOS 14+;长生效自签证书,未做公证 |
| Linux x64 | `.AppImage` / `.deb` | X11 直接可用;Wayland 走 xdg-desktop-portal |

> **macOS 首次使用**:在「系统设置 › 隐私与安全性 › 屏幕录制」中允许 Cropmark;首次打开安装包如被拦,在「隐私与安全性」里点「仍要打开」。
> **Linux Wayland**:若桌面会话没有可用的截屏门户,产品内会说明原因;X11 会话不受影响。

## 与常见工具的对比

| | Cropmark | Snipaste | PixPin | ShareX | CleanShot X | Flameshot |
| --- | :---: | :---: | :---: | :---: | :---: | :---: |
| 平台 | Win + macOS + Linux | Win + macOS | Win + macOS | 仅 Windows | 仅 macOS | Win + macOS + Linux |
| 开源 | GPL-3.0 | 否 | 否 | 是 | 否 | 是 |
| 离线 OCR(中英) | 内置模型 | 无 | 有 | 有 | 有 | 无 |
| 长截图 | 纵向 + 横向 | 无 | 有 | 有 | 有 | 无 |
| 录屏 | GIF/WebP/MP4 | 无 | 有 | 有 | 有 | 无 |
| 贴图 | 有 | 有 | 有 | 有 | 有 | 有 |
| 授权 | 完全免费 | 免费(Pro 规划中) | 基础免费 + Pro 订阅 | 免费 | 付费 | 免费 |

*对比截至 2026-10,以各产品官方页面为准。*

Cropmark 的位置:**如果你只用一个平台,总有更强的单项工具;如果你要在多个平台间工作、或者在意数据不出本机,Cropmark 是目前唯一同时满足"三平台 + 全离线 + 开源 + 功能全"的选择。**

## 快速上手

1. 安装后启动,应用常驻托盘(没有默认主窗口)
2. 按默认热键截取:`Alt+Shift+A` 区域 / `Alt+Shift+W` 窗口 / `Alt+Shift+S` 全屏
3. 截取成功进入预览:标注、取字、识别二维码、美化、复制或保存
4. 设置里可改热键、开机关联(默认关)、调整各功能开关;界面支持中文 / English

## 路线图

- [ ] 录屏声音录制(系统声音 + 麦克风)
- [ ] 应用内自动更新
- [ ] winget / Scoop / Homebrew / AUR 包管理器分发
- [ ] 更多界面语言

## 从源码构建

需要 Node.js 22+、Rust stable 及各平台 [Tauri 2 系统依赖](https://v2.tauri.app/start/prerequisites/)。仓库根目录执行 `npm install` 后,在对应主机上打包:

### Windows

```powershell
.\scripts\build-windows.ps1            # NSIS 安装包 -> src-tauri/target/release/bundle/nsis/
.\scripts\build-windows.ps1 -Debug     # 或 npm run tauri -- build --debug,产物在 src-tauri/target/debug/
```

### macOS(macOS 14+,Xcode Command Line Tools)

```bash
bash scripts/build-macos.sh
# Cropmark.app 与 .dmg -> src-tauri/target/release/bundle/{macos,dmg}/
```

### Linux

Ubuntu 22.04 示例:

```bash
sudo apt-get update
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf \
  libx11-dev libxcb1-dev libxcb-randr0-dev pkg-config
npm install
bash scripts/build-linux.sh
# AppImage 与 .deb -> src-tauri/target/release/bundle/{appimage,deb}/
```

日常开发用 `npm run tauri -- dev` 启动(带 Vite 热 reload);`npm run build` 做前端严格类型检查与产物构建。三平台配置见 `src-tauri/tauri.conf.json` 与 `src-tauri/tauri.{windows,macos,linux}.conf.json`。

## 质量与发布

[CI 工作流](.github/workflows/ci.yml) 在 PR 和 `main` 推送时检查前端构建、版本一致性、离线模型完整性、发布脚本测试,并在 Windows / macOS / Linux 三平台运行 Rust 测试和 Clippy。

发布遵循:同步 `package.json`、`package-lock.json`(两处根版本)、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock` 的 `cropmark` 条目、`src-tauri/tauri.conf.json` 的版本号 → `node .github/scripts/release.mjs check vX.Y.Z` 校验 → 提交并推送 `main` → 创建并推送 annotated tag。[发布工作流](.github/workflows/release.yml) 自动构建三平台安装包、校验 macOS 签名与四类附件完整性后发布 Release。详细研发与发布约定见 [AGENTS.md](AGENTS.md)。

## English

Cropmark is an open-source (GPL-3.0), fully offline, cross-platform screenshot suite for Windows, macOS, and Linux. It captures regions, windows, and screens via global hotkeys, and covers the full workflow in one tray-resident app:

- **Capture**: region / window / fullscreen, element snapping (UIA on Windows, AX on macOS, X11 window stack on Linux), magnifier, delay capture, multi-monitor DPI aware, original-resolution lossless output
- **Scrolling capture**: vertical and horizontal stitching with explicit per-segment control and rollback
- **Screen recording**: streaming GIF / WebP / MP4 with a HUD (countdown, pause, stop) and live annotation
- **Pin**: pin captures on top with zoom, opacity, and click-through
- **Annotate**: rect, ellipse, arrow, ink, highlighter, text, numbering, bubbles, mosaic, blur, spotlight, erase, crop, rotate
- **OCR & QR**: offline printed Chinese + English recognition with bundled models and coordinate-based text picking; local QR decoding
- **Export & beautify**: PNG / JPEG / WebP with quality and filename templates; one-click padding, rounded corners, shadow, and gradient backgrounds
- **History**: thumbnails, re-copy, save, delete with timed undo

Installers (Windows NSIS, macOS DMG, Linux AppImage/DEB) are on the [Releases](https://github.com/sakullla/cropmark/releases) page. No account, no cloud, no telemetry — everything stays on your machine. See the sections above for build instructions, or run `npm run tauri -- dev` after `npm install`.

## 许可

[GNU GPL-3.0](LICENSE)。界面、安装包、关于页和托盘只使用 **Cropmark** 名称与自有图标。
