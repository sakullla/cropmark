import { readFileSync, statSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync, spawnSync } from "node:child_process";

const ROOT = fileURLToPath(new URL("../../", import.meta.url));
const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?$/;
const INSTALLERS = [".exe", ".dmg", ".deb", ".AppImage"];
const MODELS = [
  "ch_PP-OCRv3_det_infer.onnx",
  "ch_PP-OCRv3_rec_infer.onnx",
  "ch_ppocr_mobile_v2.0_cls_infer.onnx",
];

function json(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function field(section, name) {
  return section?.match(new RegExp(`^${name}\\s*=\\s*"([^"]+)"`, "m"))?.[1];
}

export function readVersions(root = ROOT) {
  const pkg = json(resolve(root, "package.json"));
  const lock = json(resolve(root, "package-lock.json"));
  const cargo = readFileSync(resolve(root, "src-tauri/Cargo.toml"), "utf8")
    .split(/^\[/m).find((section) => section.startsWith("package]"));
  const cargoLock = readFileSync(resolve(root, "src-tauri/Cargo.lock"), "utf8")
    .split(/^\[\[package\]\]/m).find((section) => field(section, "name") === "cropmark");
  return {
    "package.json": pkg.version,
    "package-lock.json": lock.version,
    "package-lock.json packages[root]": lock.packages?.[""]?.version,
    "src-tauri/Cargo.toml": field(cargo, "version"),
    "src-tauri/Cargo.lock cropmark": field(cargoLock, "version"),
    "src-tauri/tauri.conf.json": json(resolve(root, "src-tauri/tauri.conf.json")).version,
  };
}

export function validateVersions(versions, tag) {
  const expected = versions["package.json"];
  if (typeof expected !== "string" || !SEMVER.test(expected)) {
    throw new Error("Use a SemVer version such as 0.1.0 or 0.1.0-rc.1 (without build metadata).");
  }
  for (const [path, version] of Object.entries(versions)) {
    if (version !== expected) throw new Error(`${path}: expected ${expected}, got ${version}`);
  }
  if (tag !== undefined && tag !== `v${expected}`) {
    throw new Error(`Release tag must be v${expected}, got ${tag}`);
  }
  return expected;
}

export function checkProject(tag, root = ROOT) {
  const version = validateVersions(readVersions(root), tag);
  for (const name of MODELS) {
    const path = resolve(root, "src-tauri/models", name);
    if (statSync(path).size < 1024) throw new Error(`Missing model data or Git LFS pointer: ${name}`);
  }
  return version;
}

export function formatNotes(subjects, previous) {
  const groups = { "新特性": [], "修复": [], "其他": [] };
  for (const subject of subjects) {
    const group = /^(feat|perf)(\([^)]*\))?!?:/.test(subject) ? "新特性"
      : /^fix(\([^)]*\))?!?:/.test(subject) ? "修复" : "其他";
    // Render commit subjects as plain text, without HTML or Markdown injection.
    const safe = subject.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
      .replace(/([\\`*_{}\[\]()#+.!|~])/g, "\\$1");
    groups[group].push(`- ${safe}`);
  }
  const lines = [`本版本变更（${previous ? `自 ${previous}` : "首个发布"}）：`, ""];
  for (const [heading, entries] of Object.entries(groups)) {
    if (entries.length) lines.push(`### ${heading}`, "", ...entries, "");
  }
  lines.push("---", "",
    "Windows x64：NSIS EXE；macOS 14+ Apple Silicon：DMG；Linux x64：DEB / AppImage。",
    "",
    "macOS 正式包使用固定的自签证书，不提交 Apple 公证。下载后首次打开仍需在系统设置的“隐私与安全性”中允许打开。截图需要屏幕录制权限。",
    "",
    "安装包包含离线 OCR 模型，识别过程不联网。", "");
  return lines.join("\n");
}

export function generateNotes(tag, root = ROOT) {
  checkProject(tag, root);
  // Only an ancestor of this release is eligible; unrelated newer tags are excluded.
  const previousResult = spawnSync("git", ["describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", `${tag}^`],
    { cwd: root, encoding: "utf8" });
  const previous = previousResult.status === 0 ? previousResult.stdout.trim() : "";
  const range = previous ? `${previous}..${tag}` : tag;
  const subjects = execFileSync("git", ["log", range, "--format=%s", "--no-merges"],
    { cwd: root, encoding: "utf8" }).trim().split("\n").filter(Boolean);
  return formatNotes(subjects, previous);
}

export function verifyRelease(release, tag) {
  if (!tag || release.tag_name !== tag) throw new Error("Release tag does not match this run.");
  if (release.draft !== true) throw new Error("Release must remain a draft until verification succeeds.");
  const assets = release.assets ?? [];
  for (const suffix of INSTALLERS) {
    const matches = assets.filter((asset) => /^cropmark[_-]/i.test(asset.name) && asset.name.endsWith(suffix));
    if (matches.length !== 1) throw new Error(`Expected exactly one Cropmark ${suffix} installer, found ${matches.length}.`);
    if (matches[0].state !== "uploaded" || !(matches[0].size > 0)) {
      throw new Error(`Installer is empty or upload is incomplete: ${matches[0].name}`);
    }
  }
  return assets.map((asset) => asset.name);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const [command, argument, tag] = process.argv.slice(2);
    if (command === "check") console.log(`Release metadata and OCR models verified: ${checkProject(argument)}`);
    else if (command === "notes") process.stdout.write(generateNotes(argument));
    else if (command === "verify") console.log(verifyRelease(json(argument), tag).join("\n"));
    else throw new Error("Usage: release.mjs check [tag] | notes <tag> | verify <release.json> <tag>");
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
