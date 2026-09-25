import assert from "node:assert/strict";
import test from "node:test";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { checkProject, formatNotes, generateNotes, readVersions, validateVersions, verifyRelease } from "./release.mjs";

const version = "0.1.0";
const versions = Object.fromEntries(Object.keys(readVersions()).map((key) => [key, version]));
const draft = () => ({
  tag_name: "v0.1.0", draft: true,
  assets: ["Cropmark_0.1.0_x64-setup.exe", "Cropmark_0.1.0_aarch64.dmg",
    "Cropmark_0.1.0_amd64.deb", "Cropmark_0.1.0_amd64.AppImage"]
    .map((name) => ({ name, state: "uploaded", size: 1024 })),
});

test("macOS release uses one stable self-signed identity and does not notarize", () => {
  const workflow = readFileSync(new URL("../workflows/release.yml", import.meta.url), "utf8");
  const readme = readFileSync(new URL("../../README.md", import.meta.url), "utf8");
  const agents = readFileSync(new URL("../../AGENTS.md", import.meta.url), "utf8");
  const macos = readFileSync(new URL("../../src-tauri/tauri.macos.conf.json", import.meta.url), "utf8");
  assert.match(workflow, /release app is ad-hoc/);
  assert.doesNotMatch(workflow, /grep -q '\^Signature=adhoc\$' <<< "\$signature"\s*\n\s*codesign/);
  assert.match(workflow, /test -n "\$\{APPLE_CERTIFICATE\}"/);
  assert.match(workflow, /test -n "\$\{APPLE_CERTIFICATE_PASSWORD\}"/);
  assert.match(workflow, /test -n "\$\{APPLE_SIGNING_IDENTITY\}"/);
  assert.match(workflow, /codesign --verify --deep --strict --verbose=2/);
  assert.match(workflow, /if: runner\.os != 'macOS'/);
  assert.match(workflow, /env -u APPLE_CERTIFICATE -u APPLE_CERTIFICATE_PASSWORD -u APPLE_SIGNING_IDENTITY/);
  assert.match(workflow, /TAURI_BUNDLER_DMG_IGNORE_CI=true/);
  assert.match(workflow, /--bundles dmg --no-sign/);
  assert.match(workflow, /security import/);
  assert.match(workflow, /codesign --force --options runtime --entitlements/);
  assert.doesNotMatch(workflow, /codesign --force --deep/);
  assert.match(workflow, /test -L "\$mount\/Applications"/);
  assert.match(macos, /"width": 500/);
  assert.match(macos, /"height": 320/);
  assert.match(workflow, /grep -F -x -q "Authority=\$\{APPLE_SIGNING_IDENTITY\}"/);
  assert.doesNotMatch(workflow, /CROPMARK_SIGNING_EOF/);
  assert.doesNotMatch(workflow, /APPLE_ID|APPLE_PASSWORD|APPLE_API_KEY|APPLE_API_ISSUER/);
  assert.match(readme, /固定的自签证书/);
  assert.match(readme, /不提交 Apple 公证/);
  assert.match(readme, /隐私与安全性/);
  assert.match(agents, /long-lived self-signed certificate/);
  assert.match(agents, /not notarized/);
  assert.match(agents, /Privacy & Security/);
  assert.match(macos, /"signingIdentity": "-"/);
  const notes = formatNotes(["fix: tray"], "v0.0.9");
  assert.match(notes, /固定的自签证书/);
  assert.match(notes, /不提交 Apple 公证/);
  assert.match(notes, /隐私与安全性/);
  assert.doesNotMatch(notes, /ad-hoc/);
});

test("repository versions and bundled models are consistent", () => {
  assert.equal(checkProject(), readVersions()["package.json"]);
});

test("every manifest and lockfile entry must match the release tag", () => {
  assert.equal(validateVersions(versions, "v0.1.0"), version);
  for (const key of Object.keys(versions)) {
    assert.throws(() => validateVersions({ ...versions, [key]: "0.2.0" }, "v0.1.0"));
    assert.throws(() => validateVersions({ ...versions, [key]: undefined }, "v0.1.0"));
  }
  assert.throws(() => validateVersions(versions, "v0.2.0"));
  assert.throws(() => validateVersions(versions, "0.1.0"));
});

test("prerelease versions are accepted; malformed versions are rejected", () => {
  const prerelease = Object.fromEntries(Object.keys(versions).map((key) => [key, "0.2.0-rc.1"]));
  assert.equal(validateVersions(prerelease, "v0.2.0-rc.1"), "0.2.0-rc.1");
  for (const invalid of ["01.2.0", "0.2", "0.2.0-01", "0.2.0+build", "v0.2.0"]) {
    assert.throws(() => validateVersions({ "package.json": invalid }));
  }
});

test("all four uploaded nonempty installers are required before publication", () => {
  assert.equal(verifyRelease(draft(), "v0.1.0").length, 4);
  for (let index = 0; index < 4; index++) {
    const missing = draft(); missing.assets.splice(index, 1);
    assert.throws(() => verifyRelease(missing, "v0.1.0"));
    const empty = draft(); empty.assets[index].size = 0;
    assert.throws(() => verifyRelease(empty, "v0.1.0"));
    const pending = draft(); pending.assets[index].state = "starter";
    assert.throws(() => verifyRelease(pending, "v0.1.0"));
  }
});

test("published, wrong-tag, wrong-product and duplicate assets cannot pass", () => {
  assert.throws(() => verifyRelease({ ...draft(), draft: false }, "v0.1.0"));
  assert.throws(() => verifyRelease(draft(), "v0.2.0"));
  const duplicate = draft(); duplicate.assets.push(duplicate.assets[0]);
  assert.throws(() => verifyRelease(duplicate, "v0.1.0"));
  const wrongProduct = draft(); wrongProduct.assets[0].name = "LightInk_0.1.0_x64-setup.exe";
  assert.throws(() => verifyRelease(wrongProduct, "v0.1.0"));
});

test("changelog handles breaking changes, first releases, and untrusted commit text", () => {
  const notes = formatNotes(["feat(capture)!: new capture", "fix: tray", "docs: <script>"], "v0.0.9");
  assert.match(notes, /自 v0\.0\.9/);
  assert.match(notes, /### 新特性/);
  assert.match(notes, /### 修复/);
  assert.match(notes, /### 其他/);
  assert.doesNotMatch(notes, /<script>/);
  assert.match(formatNotes([], ""), /首个发布/);
});

test("changelog selects a reachable ancestor instead of a newer unrelated tag", (t) => {
  const root = mkdtempSync(join(tmpdir(), "cropmark-release-test-"));
  t.after(() => {
    assert.equal(dirname(resolve(root)), resolve(tmpdir()));
    rmSync(root, { recursive: true, force: true });
  });
  const repo = fileURLToPath(new URL("../../", import.meta.url));
  for (const path of ["package.json", "package-lock.json", "src-tauri/Cargo.toml",
    "src-tauri/Cargo.lock", "src-tauri/tauri.conf.json"]) {
    mkdirSync(dirname(join(root, path)), { recursive: true });
    copyFileSync(join(repo, path), join(root, path));
  }
  mkdirSync(join(root, "src-tauri/models"));
  for (const name of ["ch_PP-OCRv3_det_infer.onnx", "ch_PP-OCRv3_rec_infer.onnx",
    "ch_ppocr_mobile_v2.0_cls_infer.onnx"]) {
    writeFileSync(join(root, "src-tauri/models", name), Buffer.alloc(1024));
  }
  const git = (...args) => execFileSync("git", args, { cwd: root, encoding: "utf8", stdio: "pipe" });
  git("init", "-b", "main");
  git("config", "user.name", "Release Test");
  git("config", "user.email", "release-test@example.invalid");
  git("config", "commit.gpgsign", "false");
  git("config", "tag.gpgsign", "false");
  git("add", ".");
  git("commit", "-m", "feat: initial version");
  const currentTag = `v${checkProject(undefined, root)}`;
  git("tag", currentTag);
  assert.match(generateNotes(currentTag, root), /首个发布/);
  git("tag", "-d", currentTag);
  git("tag", "v0.0.1");
  git("checkout", "-b", "unrelated");
  git("commit", "--allow-empty", "-m", "feat: unrelated branch");
  git("tag", "v99.0.0");
  git("checkout", "main");
  git("commit", "--allow-empty", "-m", "fix: current release");
  git("tag", currentTag);
  const notes = generateNotes(currentTag, root);
  assert.match(notes, /自 v0\.0\.1/);
  assert.match(notes, /current release/);
  assert.doesNotMatch(notes, /unrelated branch|initial version/);
});
