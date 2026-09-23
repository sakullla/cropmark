import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const appCss = readFileSync(new URL("../../src/styles/app.css", import.meta.url), "utf8");
const historyCss = readFileSync(new URL("../../src/history/history.css", import.meta.url), "utf8");
const toastCss = readFileSync(new URL("../../src/toast/toast.css", import.meta.url), "utf8");
const indexHtml = readFileSync(new URL("../../src/index.html", import.meta.url), "utf8");

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
