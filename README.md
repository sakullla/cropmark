# Cropmark

独立系统截图工具。托盘常驻，热键截取区域、窗口或全屏，预览里标注、复制、保存，并离线识别印刷体中文和英文。

界面、安装包、关于页和托盘只使用 **Cropmark** 名称与自有图标。

许可：[GNU GPL-3.0](LICENSE)

## 构建

需要 Node.js 22+、Rust stable，以及各平台的 [Tauri 2 系统依赖](https://v2.tauri.app/start/prerequisites/)。仓库根目录执行 `npm install` 后，在对应主机上打包：

### Windows

```powershell
npm install
.\scripts\build-windows.ps1
```

产出 NSIS 安装包：`src-tauri/target/release/bundle/nsis/`（显示名 Cropmark）。

本机调试构建（不经过安装包）：

```powershell
.\scripts\build-windows.ps1 -Debug
# 或
npm run tauri -- build --debug
```

调试可执行文件：`src-tauri/target/debug/cropmark.exe`。

### macOS

macOS 14+，需 Xcode Command Line Tools。

```bash
npm install
bash scripts/build-macos.sh
```

产出 `Cropmark.app` 与 `.dmg`：

- `src-tauri/target/release/bundle/macos/`
- `src-tauri/target/release/bundle/dmg/`

首次截取前请在「系统设置 › 隐私与安全性 › 屏幕录制」中打开 Cropmark。

### Linux

需要 `libwebkit2gtk-4.1`、托盘指示器、`patchelf`，以及截屏用的 `libx11` / `libxcb`（X11）或会话中的 xdg-desktop-portal（Wayland）。Ubuntu 22.04 示例：

```bash
sudo apt-get update
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf \
  libx11-dev libxcb1-dev libxcb-randr0-dev pkg-config
npm install
bash scripts/build-linux.sh
```

产出 AppImage 与 `.deb`：

- `src-tauri/target/release/bundle/appimage/`
- `src-tauri/target/release/bundle/deb/`

Wayland 下若门户不可用，产品内会说明原因；X11 可走 `XGetImage`。

三平台配置在 `src-tauri/tauri.conf.json` 与 `src-tauri/tauri.{windows,macos,linux}.conf.json`。GitHub Actions 工作流 [`.github/workflows/packages.yml`](.github/workflows/packages.yml) 在 Windows、macOS、Ubuntu 上调用上述脚本并上传 Cropmark 工件。

## 使用

启动后没有默认主窗口，只出现托盘或菜单栏。默认热键：

- 区域 `Alt+Shift+A`
- 窗口 `Alt+Shift+W`
- 全屏 `Alt+Shift+S`

截取成功后打开预览，未标注 PNG 进入剪贴板。设置里可改热键、开关开机启动（默认关）。OCR 模型打进安装包，识别不联网。
