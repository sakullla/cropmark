# Repository Guidelines

## Project Structure & Module Organization

Cropmark is a Tauri 2 desktop screenshot utility with tray residency, global hotkeys, annotation, clipboard/export, and offline Chinese/English OCR. TypeScript lives in `src/`, organized into `overlay/`, `preview/`, `settings/`, `errors/`, and shared `styles/`. `src/main.ts` mounts the view selected by the window URL. The Rust backend is in `src-tauri/src/`: `capture/` owns capture sessions and platform implementations, `annotate/` rasterizes annotations, and `ocr/` loads local models. Tray, hotkeys, settings, autostart, clipboard, and export each have their own modules.

Tauri configuration and OS overrides live in `src-tauri/tauri*.conf.json`; capabilities are in `src-tauri/capabilities/`. Keep all three ONNX models in `src-tauri/models/` available to packaged apps. Active app icons are in `src-tauri/icons/v2/`; artwork and its exporter are in `assets/brand/`. `output/imagegen/` contains the generated source and prompt. Requirements live in `docs/requirements/` when present; `docs/` is currently ignored by Git, so check tracking before treating a document as a shared deliverable.

## Build, Test, and Development Commands

Run from the Cropmark repository root. Use Node.js 22+ and Rust stable with the platform's Tauri 2 prerequisites.

- `npm ci`: install the locked JavaScript dependencies; use `npm install` when intentionally changing dependencies.
- `npm run tauri -- dev`: launch the desktop application with Vite hot reload. It starts in the tray rather than opening a default main window.
- `npm run dev`: run only the frontend on port 1420; native capture, clipboard, hotkeys, and OCR require Tauri.
- `npm run build`: run strict TypeScript checks and produce `dist/` with Vite.
- `cargo check --locked --manifest-path src-tauri/Cargo.toml`: check the backend for the current host.
- `cargo test --locked --manifest-path src-tauri/Cargo.toml`: run Rust unit and documentation tests.
- `cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets`: run backend lint checks. Existing warnings are not promoted to errors in CI.
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`: inspect Rust formatting. The repository has pre-existing formatting differences; this is not currently a CI gate. Keep formatting edits scoped to touched code.
- `node .github/scripts/release.mjs check`: verify release versions and bundled OCR model files.
- `node --test .github/scripts/release.test.mjs`: test release validation and changelog behavior.
- `powershell -File scripts/build-windows.ps1`: build the Windows NSIS installer; add `-Debug` for a debug build.
- `bash scripts/build-macos.sh`: build the macOS app and DMG on macOS 14+ with Xcode Command Line Tools.
- `bash scripts/build-linux.sh`: build the Linux AppImage and DEB on Linux with the documented system dependencies.

Use this checkout and its `src-tauri/target` incremental cache. Do not redirect `CARGO_TARGET_DIR` to a sandbox or temporary copy without a concrete need. Frontend-only edits normally require a frontend rebuild or window reload, not a full Rust rebuild. Local helper scripts use `src-tauri/target/release/bundle/`; release CI passes an explicit Rust target and uses `src-tauri/target/<target>/release/bundle/`.

## Coding Style & Naming Conventions

Use strict TypeScript, ES modules, two-space indentation, semicolons, and double quotes, following the existing files. Use camelCase for variables/functions, PascalCase for types, and feature-owned modules instead of broad utility files. Rust follows snake_case names and rustfmt conventions. No frontend test runner, ESLint, or standalone frontend formatter is configured; `npm run build` is the frontend static check. Do not document or invoke nonexistent `npm test`, `tauri:dev`, or `tauri:build` scripts.

Keep platform-specific behavior behind the existing Rust modules and `cfg` boundaries. Preserve the tray-resident lifecycle, hide-before-capture behavior, monitor/DPI coordinate handling, and cancellation semantics. OCR must continue to work offline from bundled models. Do not introduce runtime model downloads. Keep user-facing names, installer metadata, and icons consistent with Cropmark.

## Testing Guidelines

Rust tests live beside implementation in `#[cfg(test)]` modules. Add focused behavioral or regression tests when logic changes; run the relevant targeted tests before the full suite for changes across modules. Release script tests use Node's built-in test runner and require no extra dependencies. `.github/workflows/ci.yml` checks frontend builds, release metadata/models, release script tests, and Rust tests/Clippy on Windows, macOS, and Linux; the release workflow reuses those checks.

Build success is not a screenshot or desktop integration test. For native UI changes, exercise the affected hotkey/tray flow, monitor scaling, cancellation, clipboard/export, or offline OCR on the relevant platform. Some OCR tests skip recognition when their local sample is unavailable; a passing suite does not establish OCR accuracy. Report what was actually tested and any platform verification still needed. Do not require a repository-wide formatting rewrite or `-D warnings` as part of an unrelated fix.

## Commit & Pull Request Guidelines

Use concise, scoped commit subjects with Conventional Commit prefixes such as `feat(capture):`, `fix(preview):`, `docs:`, or `ci:`. Release notes group `feat`/`perf`, `fix`, and other commits automatically. Pull requests should explain the resulting behavior, list meaningful validation, link relevant requirements, and include visual evidence for UI changes. Call out OS-specific behavior and material limitations. Preserve unrelated work in the working tree.

## Version Release Process

Keep the release version identical in `package.json`, both root version entries in `package-lock.json`, `src-tauri/Cargo.toml`, the `cropmark` package in `src-tauri/Cargo.lock`, and `src-tauri/tauri.conf.json`. Use SemVer such as `0.1.1` or `0.1.1-rc.1`, without build metadata. Run the release checker with the intended tag, for example `node .github/scripts/release.mjs check v0.1.1`, plus the frontend build, release script tests, Cargo tests, and Clippy before tagging.

When a release is requested, commit the version change with a subject such as `chore(release): v0.1.1`, push `main`, create an annotated tag with `git tag -a v0.1.1 -m "Cropmark v0.1.1"`, and push it with `git push origin v0.1.1`. Preparing workflow files or documentation alone is not an instruction to tag or publish.

`.github/workflows/release.yml` owns Release creation, uploads, and publication. Do not manually publish a Release before the workflow completes. A `v*` tag triggers validation, creates or resumes one draft, builds Windows x64 / macOS ARM64 / Linux x64 installers, and publishes only after all platform jobs succeed and exactly one nonempty uploaded `.exe`, `.dmg`, `.deb`, and `.AppImage` asset has been verified. Tags with a SemVer prerelease suffix create GitHub prereleases. Cropmark currently ships NSIS on Windows, not MSI, and does not target Android.

macOS bundles use explicit ad-hoc signing and are not notarized. The workflow verifies the signature before publication. A failed build leaves the draft unpublished; rerun the failed workflow jobs for the same unchanged tag. Never move a release tag or overwrite a published release; code fixes require a new patch version. Confirm the `Verify release assets and publish` job succeeds and inspect the public download assets before calling the release complete.

Keep `AGENTS.md` and `CLAUDE.md` synchronized when changing repository guidance.
