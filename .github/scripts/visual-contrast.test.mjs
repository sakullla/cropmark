import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const appCss = readFileSync(new URL("../../src/styles/app.css", import.meta.url), "utf8");
const historyCss = readFileSync(new URL("../../src/history/history.css", import.meta.url), "utf8");
const toastCss = readFileSync(new URL("../../src/toast/toast.css", import.meta.url), "utf8");
const indexHtml = readFileSync(new URL("../../src/index.html", import.meta.url), "utf8");
const overlayCss = readFileSync(new URL("../../src/overlay/overlay.css", import.meta.url), "utf8");
const overlayIndex = readFileSync(new URL("../../src/overlay/index.ts", import.meta.url), "utf8");
const previewCss = readFileSync(new URL("../../src/preview/preview.css", import.meta.url), "utf8");
const previewIndex = readFileSync(new URL("../../src/preview/index.ts", import.meta.url), "utf8");
const editorCss = readFileSync(new URL("../../src/annotation/editor.css", import.meta.url), "utf8");
const annotationIndex = readFileSync(new URL("../../src/annotation/index.ts", import.meta.url), "utf8");
const pinCss = readFileSync(new URL("../../src/pin/pin.css", import.meta.url), "utf8");

const TEXT_PAIRS = [
  ["ink", "bg"],
  ["ink", "panel"],
  ["ink", "card"],
  ["ink", "control-fill"],
  ["ink", "accent-soft"],
  ["ink", "danger-soft"],
  ["muted", "bg"],
  ["muted", "panel"],
  ["muted", "card"],
  ["muted", "control-fill"],
  ["accent", "bg"],
  ["accent", "panel"],
  ["accent", "card"],
  ["accent", "accent-soft"],
  ["accent", "control-fill"],
  ["danger", "bg"],
  ["danger", "panel"],
  ["danger", "card"],
  ["danger", "danger-soft"],
  ["danger", "control-fill"],
  ["ok", "bg"],
  ["ok", "panel"],
  ["ok", "card"],
  ["ok", "ok-soft"],
  ["ok", "control-fill"],
  ["on-accent", "accent"],
  ["on-danger", "danger"],
];

const UI_PAIRS = [
  ["line", "panel"],
  ["line", "card"],
  ["line", "control-fill"],
  ["accent", "control-fill"],
  ["accent", "accent-soft"],
  ["accent", "panel"],
  ["accent", "card"],
  ["danger", "control-fill"],
  ["danger", "danger-soft"],
  ["danger", "panel"],
  ["danger", "card"],
  ["knob", "track"],
  ["knob", "accent"],
  ["track", "panel"],
  ["track", "card"],
  ["focus", "focus-gap"],
  ["focus", "panel"],
  ["focus", "control-fill"],
  ["focus", "card"],
  ["focus", "accent-soft"],
  ["focus", "danger-soft"],
  ["focus-gap", "accent"],
  ["focus-gap", "danger"],
  ["focus-gap", "track"],
];

function stripComments(css) {
  return css.replace(/\/\*[\s\S]*?\*\//g, "");
}

function blockAt(css, openBrace) {
  let depth = 0;
  for (let index = openBrace; index < css.length; index += 1) {
    const char = css[index];
    if (char === "{") depth += 1;
    else if (char === "}") {
      depth -= 1;
      if (depth === 0) return { body: css.slice(openBrace + 1, index), end: index + 1 };
    }
  }
  throw new Error("unclosed CSS block");
}

function rulesIn(css) {
  const rules = [];
  let index = 0;
  while (index < css.length) {
    while (index < css.length && /\s/.test(css[index])) index += 1;
    if (index >= css.length) break;
    const open = css.indexOf("{", index);
    if (open < 0) break;
    const selector = css.slice(index, open).trim();
    const block = blockAt(css, open);
    if (selector.startsWith("@")) rules.push(...rulesIn(block.body));
    else rules.push({ selector, body: block.body });
    index = block.end;
  }
  return rules;
}

function readDeclarations(body) {
  const tokens = {};
  for (const match of body.matchAll(/--([a-z0-9-]+)\s*:\s*([^;]+);/g)) {
    tokens[match[1]] = match[2].trim();
  }
  return tokens;
}

function themesFrom(css) {
  const source = stripComments(css);
  const rules = rulesIn(source);
  const lightRule = rules.find((rule) => rule.selector === ":root");
  assert.ok(lightRule, "app.css is missing the light :root token block");
  const darkMedia = source.match(/@media\s*\(\s*prefers-color-scheme:\s*dark\s*\)\s*\{/);
  assert.ok(darkMedia, "app.css is missing prefers-color-scheme: dark");
  const darkBlock = blockAt(source, source.indexOf("{", darkMedia.index));
  const darkRoot = rulesIn(darkBlock.body).find((rule) => rule.selector === ":root");
  assert.ok(darkRoot, "dark theme is missing a :root token block");
  const light = readDeclarations(lightRule.body);
  return { light, dark: { ...light, ...readDeclarations(darkRoot.body) } };
}

function channel(value) {
  const scaled = value / 255;
  return scaled <= 0.04045 ? scaled / 12.92 : ((scaled + 0.055) / 1.055) ** 2.4;
}

function luminance(hex) {
  const match = /^#([0-9a-f]{6})$/i.exec(hex);
  assert.ok(match, `expected a #rrggbb token, got ${hex}`);
  const raw = match[1];
  const red = channel(Number.parseInt(raw.slice(0, 2), 16));
  const green = channel(Number.parseInt(raw.slice(2, 4), 16));
  const blue = channel(Number.parseInt(raw.slice(4, 6), 16));
  return 0.2126 * red + 0.7152 * green + 0.0722 * blue;
}

function contrast(foreground, background) {
  const lighter = Math.max(luminance(foreground), luminance(background));
  const darker = Math.min(luminance(foreground), luminance(background));
  return (lighter + 0.05) / (darker + 0.05);
}

function assertPairs(tokens, pairs, minimum, label) {
  for (const [foreground, background] of pairs) {
    const fg = tokens[foreground];
    const bg = tokens[background];
    assert.ok(fg && bg, `${label} is missing --${foreground} or --${background}`);
    const ratio = contrast(fg, bg);
    assert.ok(
      ratio >= minimum,
      `${label} --${foreground} on --${background} is ${ratio.toFixed(2)}:1, below ${minimum}:1`,
    );
  }
}

function selectorItems(selector) {
  return selector.split(",").map((item) => item.trim().replace(/\s+/g, " "));
}

function assertMinTarget(css, selectors) {
  const rules = rulesIn(stripComments(css));
  for (const selector of selectors) {
    const matched = rules.some((rule) => (
      selectorItems(rule.selector).includes(selector)
      && /min-width:\s*24px/.test(rule.body)
      && /min-height:\s*24px/.test(rule.body)
    ));
    assert.ok(matched, `${selector} is missing a 24px minimum width and height`);
  }
}

function ruleBodies(css, selector) {
  const matched = rulesIn(stripComments(css)).filter((rule) => (
    selectorItems(rule.selector).includes(selector)
  ));
  assert.ok(matched.length > 0, `${selector} is missing`);
  return matched.map((rule) => rule.body).join("\n");
}

function lastPx(body, property) {
  const matches = [...body.matchAll(new RegExp(`${property}\\s*:\\s*(\\d+(?:\\.\\d+)?)px`, "g"))];
  return matches.length === 0 ? null : Number(matches.at(-1)[1]);
}

function targetFloor(body, minimumProperty, sizeProperty) {
  const values = [lastPx(body, minimumProperty), lastPx(body, sizeProperty)].filter((value) => value !== null);
  assert.ok(values.length > 0, `missing ${minimumProperty} or ${sizeProperty}`);
  return Math.max(...values);
}

function assertClickTarget(css, selectors) {
  assert.doesNotMatch(
    css,
    /(?:min-width|min-height|width|height)\s*:\s*44px/,
    "click targets must stay below 44px",
  );
  for (const selector of selectors) {
    const body = ruleBodies(css, selector);
    const width = targetFloor(body, "min-width", "width");
    const height = targetFloor(body, "min-height", "height");
    assert.ok(width >= 24, `${selector} width target is ${width}px`);
    assert.ok(height >= 24, `${selector} height target is ${height}px`);
  }
}

test("contrast helper keeps the WCAG floor meaningful", () => {
  assert.ok(contrast("#000000", "#ffffff") >= 20);
  assert.ok(contrast("#ffffff", "#ffffff") < 1.1);
  assert.ok(contrast("#cccccc", "#ffffff") < 3);
});

test("light and dark text and UI tokens meet the contrast floor", () => {
  const { light, dark } = themesFrom(appCss);
  assertPairs(light, TEXT_PAIRS, 4.5, "light text");
  assertPairs(dark, TEXT_PAIRS, 4.5, "dark text");
  assertPairs(light, UI_PAIRS, 3, "light ui");
  assertPairs(dark, UI_PAIRS, 3, "dark ui");
  assert.equal(light["tooltip-bg"], dark["tooltip-bg"]);
  assert.equal(light["tooltip-ink"], dark["tooltip-ink"]);
  assert.ok(contrast(light["tooltip-ink"], light["tooltip-bg"]) >= 4.5);
  assert.equal(light["overlay-bg"], "#111110");
  assert.equal(dark["overlay-bg"], "#111110");
});

test("focus stays an inset indicator inside the clipped shell", () => {
  const rules = rulesIn(stripComments(appCss));
  const focusRules = rules.filter((rule) => selectorItems(rule.selector).some((item) => (
    item === ":focus-visible" || item.endsWith(":focus-visible")
  )));
  assert.ok(focusRules.length > 0, "missing a :focus-visible rule");
  assert.ok(
    focusRules.every((rule) => /outline-offset:\s*-\d/.test(rule.body) && /inset/.test(rule.body)),
    "focus-visible must use a negative outline offset and an inset shadow",
  );
  assert.ok(rules.some((rule) => rule.selector === ".shell" && /overflow:\s*hidden/.test(rule.body)));
});

test("shared controls and history buttons keep a 24px minimum target", () => {
  assertMinTarget(appCss, [".icon-btn", ".hotkey-btn", ".choice", ".switch", ".number-input"]);
  assertMinTarget(historyCss, [
    ".history-toolbar .choice",
    ".history-toolbar .icon-btn",
    ".history-actions .choice",
    ".history-confirm-actions .choice",
  ]);
});

test("reduced motion removes nonessential transitions", () => {
  const source = stripComments(appCss);
  const media = source.match(/@media\s*\(\s*prefers-reduced-motion:\s*reduce\s*\)/);
  assert.ok(media, "missing prefers-reduced-motion");
  const block = blockAt(source, source.indexOf("{", media.index));
  assert.match(block.body, /\.switch\s+\.knob/);
  assert.match(block.body, /\[data-tooltip\]::after/);
  assert.match(block.body, /transition\s*:\s*none/);
});

test("overlay and tooltip colors come from tokens, and toast text can wrap", () => {
  assert.match(indexHtml, /background:\s*var\(\s*--overlay-bg\s*\)/);
  assert.doesNotMatch(indexHtml, /#111110/i);
  assert.match(appCss, /background:\s*var\(\s*--tooltip-bg\s*\)/);
  assert.match(appCss, /color:\s*var\(\s*--tooltip-ink\s*\)/);
  assert.match(toastCss, /white-space:\s*normal/);
  assert.doesNotMatch(toastCss, /white-space:\s*nowrap/);
  assert.doesNotMatch(toastCss, /text-overflow:\s*ellipsis/);
});

test("overlay and preview keep existing controls on the frame edge", () => {
  for (const snippet of [
    'class="overlay-confirm"',
    'class="overlay-capabilities"',
    'class="overlay-cancel"',
    'class="overlay-tools annotation-tools"',
    'class="window-list"',
  ]) {
    assert.ok(overlayIndex.includes(snippet), snippet);
  }
  for (const snippet of [
    'class="preview-close icon-btn"',
    "data-annotation-toolbar",
    'data-tool="ocr"',
    'data-action="copy-ocr-all"',
    'data-action="pin"',
    'data-action="update-pin"',
    'data-action="save"',
    'data-action="toggle-quality"',
    'data-action="copy"',
  ]) {
    assert.ok(previewIndex.includes(snippet), snippet);
  }
  assert.match(
    annotationIndex,
    /export const STYLE_COLORS = \["#e11d48", "#2563eb", "#f59e0b", "#10b981", "#111827"\];/,
  );

  const chrome = ruleBodies(overlayCss, ".overlay-chrome");
  const tools = ruleBodies(overlayCss, ".overlay-tools");
  assert.match(chrome, /top:\s*16px/);
  assert.doesNotMatch(chrome, /top:\s*50%/);
  assert.match(tools, /bottom:\s*20px/);
  assert.doesNotMatch(tools, /top:\s*50%/);
  assert.match(ruleBodies(previewCss, ".preview-toolbar"), /flex:\s*0\s+0\s+auto/);
  assert.doesNotMatch(ruleBodies(previewCss, ".preview-toolbar"), /position:\s*absolute/);
  assert.match(ruleBodies(previewCss, ".preview-stage"), /flex:\s*1/);
  const pinBar = ruleBodies(pinCss, ".pin-toolbar");
  assert.match(pinBar, /top:\s*6px/);
  assert.match(pinBar, /right:\s*6px/);
});

test("selection stroke uses a token plus a halo token beside it", () => {
  assert.match(overlayCss, /--selection-stroke:\s*var\(\s*--accent\s*\)/);
  assert.match(overlayCss, /--selection-halo:\s*var\(\s*--focus-gap\s*\)/);
  assert.match(overlayIndex, /getPropertyValue\(name\)/);
  assert.match(overlayIndex, /"--selection-stroke"/);
  assert.match(overlayIndex, /"--selection-halo"/);
  assert.match(
    overlayIndex,
    /strokeRect\(\s*x - lineWidth,\s*y - lineWidth,\s*width \+ lineWidth \* 2,\s*height \+ lineWidth \* 2\s*\)/,
  );
  assert.match(overlayIndex, /strokeRect\(\s*x,\s*y,\s*width,\s*height\s*\)/);
  assert.doesNotMatch(overlayIndex, /#2dd4bf/i);
  assert.doesNotMatch(overlayIndex, /45\s*,\s*212\s*,\s*191/);
  const { light, dark } = themesFrom(appCss);
  for (const [label, tokens] of [["light", light], ["dark", dark]]) {
    const ratio = contrast(tokens.accent, tokens["focus-gap"]);
    assert.ok(ratio >= 3, `${label} selection stroke on its halo is ${ratio.toFixed(2)}:1`);
  }
});

test("pin toolbar colors come from tokens and the opacity fade stays 120ms", () => {
  for (const selector of [
    ".pin-toolbar button",
    ".pin-toolbar button:hover",
    ".pin-toolbar .pin-close:hover",
  ]) {
    const body = ruleBodies(pinCss, selector);
    assert.match(body, /var\(\s*--/);
    assert.doesNotMatch(body, /#[0-9a-f]{3,8}/i);
    assert.doesNotMatch(body, /rgba?\(/);
  }
  assert.match(ruleBodies(pinCss, ".pin-toolbar"), /transition:\s*opacity\s+120ms\s+ease/);
  assert.doesNotMatch(pinCss, /prefers-reduced-motion[\s\S]*\.pin-toolbar[\s\S]*transition\s*:\s*none/);
});

test("preview note pulse is at most 200ms, does not scale, and reduced motion stops it", () => {
  const source = stripComments(previewCss);
  assert.match(ruleBodies(previewCss, ".preview-note.is-pulse"), /animation:\s*preview-note-pulse\s+200ms\s+ease/);
  const frames = source.match(/@keyframes\s+preview-note-pulse\s*\{/);
  assert.ok(frames, "missing preview-note-pulse");
  const block = blockAt(source, source.indexOf("{", frames.index));
  assert.doesNotMatch(block.body, /scale\s*\(/);
  assert.doesNotMatch(block.body, /transform\s*:/);
  const declared = /preview-note-pulse\s+(\d+(?:\.\d+)?)(ms|s)/.exec(source);
  assert.ok(declared, "missing preview-note-pulse duration");
  const duration = Number(declared[1]) * (declared[2] === "s" ? 1000 : 1);
  assert.ok(duration <= 200, `preview-note-pulse is ${duration}ms`);
  const media = source.match(/@media\s*\(\s*prefers-reduced-motion:\s*reduce\s*\)/);
  assert.ok(media, "preview is missing prefers-reduced-motion");
  const motion = blockAt(source, source.indexOf("{", media.index));
  assert.match(motion.body, /\.preview-note\.is-pulse/);
  assert.match(motion.body, /animation\s*:\s*none/);
});

test("overlay, preview, pin, and annotation click targets are at least 24px", () => {
  assertClickTarget(overlayCss, [
    ".overlay-cancel",
    ".overlay-confirm",
    ".overlay-capabilities",
    ".window-item",
    ".delay-root button",
    ".error-root button",
  ]);
  assertClickTarget(previewCss, [
    ".preview-close",
    ".preview-actions button",
    ".preview-save-caret",
  ]);
  assertClickTarget(editorCss, [
    ".annotation-tools button",
    ".style-options button",
    ".style-options button[data-style-color]",
    ".style-options input[data-style-number-start]",
    ".annotation-context button",
  ]);
  assertClickTarget(pinCss, [
    ".pin-toolbar button",
    ".pin-menu button",
    ".pin-menu-opacity button",
  ]);
});
