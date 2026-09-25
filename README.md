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

三平台配置在 `src-tauri/tauri.conf.json` 与 `src-tauri/tauri.{windows,macos,linux}.conf.json`。本地脚本按当前主机架构打包；GitHub Release 固定提供 Windows x64、macOS Apple Silicon、Linux x64 安装包。macOS 正式包使用固定的自签证书，不提交 Apple 公证。下载后首次打开仍需在「隐私与安全性」中放行。没有证书环境变量时，本机调试构建仍使用 ad-hoc 签名。

## 检查与发布

[CI 工作流](.github/workflows/ci.yml) 在 PR 和 `main` 推送时检查前端构建、版本一致性、离线模型、发布脚本测试，并在三平台运行 Rust 测试和 Clippy。项目目前没有前端测试运行器；已有 Rust 格式差异和告警尚未设为阻断项。

发布前同步以下版本：`package.json`、`package-lock.json` 的两个根版本、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock` 的 `cropmark` 条目，以及 `src-tauri/tauri.conf.json`。以 `0.1.1` 为例，执行：

```powershell
node .github/scripts/release.mjs check v0.1.1
node --test .github/scripts/release.test.mjs
npm run build
cargo test --locked --manifest-path src-tauri/Cargo.toml
cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets
```

提交版本变更并推送 `main` 后，创建并推送 annotated tag：

```powershell
git tag -a v0.1.1 -m "Cropmark v0.1.1"
git push origin v0.1.1
```

[发布工作流](.github/workflows/release.yml) 复用 CI 检查，按提交生成变更记录，建立草稿 Release，并上传 EXE、DMG、DEB、AppImage。只有三平台构建、macOS 签名检查及四种附件完整性检查全部通过后才公开发布。`v0.1.1-rc.1` 这类标签发布为预发布版本。

失败时草稿保持未发布，可在相同且未修改的 tag 上重跑失败任务。不要提前手动发布 Release，也不要移动已有 tag；修复代码后发布新的补丁版本。发布成功以 `Verify release assets and publish` 任务通过且下载附件齐全为准。研发与发布约定见 [AGENTS.md](AGENTS.md) 和 [CLAUDE.md](CLAUDE.md)。

## 使用

启动后没有默认主窗口，只出现托盘或菜单栏。默认热键：

- 区域 `Alt+Shift+A`
- 窗口 `Alt+Shift+W`
- 全屏 `Alt+Shift+S`

截取成功后打开预览，未标注 PNG 进入剪贴板。设置里可改热键、开关开机启动（默认关）。OCR 模型打进安装包，识别不联网。

截图保留选区或屏幕的原始物理像素尺寸。预览会适应当前屏幕的工作区，这只改变显示比例；复制和保存仍使用原始分辨率的无损 PNG，不会自动缩小图片。
