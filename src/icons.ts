/** 全应用共用的线性图标:24×24、圆头描边,预览/覆盖层/贴图/设置/历史同一套。 */

function svg(body: string): string {
  return `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
}

export const icons = {
  rect: svg(`<rect x="4.5" y="6.2" width="15" height="11.6" rx="2.4"/>`),
  ellipse: svg(`<ellipse cx="12" cy="12" rx="7.4" ry="5.8"/>`),
  line: svg(
    `<path d="M6.2 17.6 17.8 6.4"/><circle cx="6.2" cy="17.6" r="1.45" fill="currentColor" stroke="none"/><circle cx="17.8" cy="6.4" r="1.45" fill="currentColor" stroke="none"/>`,
  ),
  arrow: svg(
    `<path d="M6 17.8 14.6 9.2"/><path d="M10.2 7.2h7.2v7.2" fill="none"/><path d="M17.4 7.2 14.6 9.2" fill="none"/>`,
  ),
  mosaic: svg(
    `<g fill="currentColor" stroke="none"><rect x="4.2" y="4.2" width="4.4" height="4.4" rx="0.8"/><rect x="9.8" y="4.2" width="4.4" height="4.4" rx="0.8" opacity="0.42"/><rect x="15.4" y="4.2" width="4.4" height="4.4" rx="0.8"/><rect x="4.2" y="9.8" width="4.4" height="4.4" rx="0.8" opacity="0.42"/><rect x="9.8" y="9.8" width="4.4" height="4.4" rx="0.8"/><rect x="15.4" y="9.8" width="4.4" height="4.4" rx="0.8" opacity="0.28"/><rect x="4.2" y="15.4" width="4.4" height="4.4" rx="0.8"/><rect x="9.8" y="15.4" width="4.4" height="4.4" rx="0.8" opacity="0.28"/><rect x="15.4" y="15.4" width="4.4" height="4.4" rx="0.8"/></g>`,
  ),
  blur: svg(
    `<circle cx="12" cy="12" r="8" fill="currentColor" stroke="none" opacity="0.16"/><circle cx="12" cy="12" r="5.2" fill="currentColor" stroke="none" opacity="0.34"/><circle cx="12" cy="12" r="2.3" fill="currentColor" stroke="none"/>`,
  ),
  highlighter: svg(
    `<path d="M8.2 3.8h5.4c.8 0 1.4.6 1.4 1.4V10H8.2z" fill="currentColor" stroke="none" opacity="0.38"/><path d="M7.4 10h8.4v2.4c0 .7-.6 1.3-1.3 1.3H8.7c-.7 0-1.3-.6-1.3-1.3z" fill="currentColor" stroke="none"/><path d="M8.4 13.7 6.6 20M15.6 13.7 17.4 20"/><path d="M6.6 20h10.8"/>`,
  ),
  pen: svg(
    `<path d="M14.2 4.6 19.4 9.8"/><path d="M13.2 5.6 18.4 10.8 8.6 20.6H3.4v-5.2z"/><path d="M11.8 7 17 12.2"/>`,
  ),
  number: svg(
    `<circle cx="12" cy="12" r="7.5"/><path d="M10.6 8.6 12.8 7.6v9.2"/>`,
  ),
  text: svg(`<path d="M5.8 7.1h12.4M12 7.1v10.4M8.6 17.5h6.8"/>`),
  undo: svg(`<path d="M5.4 10.2h8.4a4.2 4.2 0 1 1 0 8.4h-1.6"/><path d="M5.4 10.2 8.8 6.8M5.4 10.2 8.8 13.6"/>`),
  redo: svg(`<path d="M18.6 10.2H10.2a4.2 4.2 0 1 0 0 8.4h1.6"/><path d="M18.6 10.2 15.2 6.8M18.6 10.2 15.2 13.6"/>`),
  style: svg(
    `<path d="M12 3.6a8.4 8.4 0 1 0 0 16.8c1.15 0 1.85-.85 1.85-1.8 0-1.35 1.2-1.85 2.75-1.85h1.4c1.2 0 2.0-1.0 2.0-2.55A8.4 8.4 0 0 0 12 3.6Z"/><circle cx="8.2" cy="10.4" r="1.15" fill="currentColor" stroke="none"/><circle cx="12.3" cy="7.8" r="1.15" fill="currentColor" stroke="none"/><circle cx="16.2" cy="11.2" r="1.15" fill="currentColor" stroke="none"/>`,
  ),
  copy: svg(
    `<rect x="8.4" y="8.4" width="10.2" height="10.2" rx="2"/><path d="M15.6 8.4V6.2A2.2 2.2 0 0 0 13.4 4H6.2A2.2 2.2 0 0 0 4 6.2v7.2A2.2 2.2 0 0 0 6.2 15.6H8.4"/>`,
  ),
  save: svg(
    `<path d="M12 4.6v10.2"/><path d="M8.2 11.2 12 15.2 15.8 11.2"/><path d="M5.4 18.6h13.2"/>`,
  ),
  pin: svg(
    `<path d="M12 19.6v-5.4"/><path d="M8.2 10.2a3.8 3.8 0 1 1 7.6 0c0 2.1-1.65 3.9-3.8 6-2.15-2.1-3.8-3.9-3.8-6Z"/><circle cx="12" cy="10" r="1.2" fill="currentColor" stroke="none"/>`,
  ),
  annotate: svg(
    `<path d="M13.1 5.6 18.4 10.9"/><path d="M4.4 19.6 6 14.6 16.6 4a2.05 2.05 0 0 1 2.9 2.9L8.9 17.5z"/>`,
  ),
  ocr: svg(
    `<rect x="6.2" y="4.4" width="11.6" height="15.2" rx="2"/><path d="M8.8 8.8h6.4M8.8 12.2h6.4M8.8 15.6h4.2"/>`,
  ),
  rotate: svg(
    `<path d="M19 11.2a7 7 0 1 1-2.1-5.1"/><path d="M19.2 3.6v4.8h-4.8"/>`,
  ),
  more: svg(`<circle cx="5.2" cy="12" r="1.45" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1.45" fill="currentColor" stroke="none"/><circle cx="18.8" cy="12" r="1.45" fill="currentColor" stroke="none"/>`),
  chevronDown: svg(`<path d="M6.4 9.2 12 14.6 17.6 9.2"/>`),
  close: svg(`<path d="M6.6 6.6 17.4 17.4M17.4 6.6 6.6 17.4"/>`),
  trash: svg(
    `<path d="M5.2 7.4h13.6"/><path d="M9.4 7.4V5.6A1.6 1.6 0 0 1 11 4h2a1.6 1.6 0 0 1 1.6 1.6v1.8"/><path d="M8 7.4 8.8 19.2A1.7 1.7 0 0 0 10.5 20.8h3a1.7 1.7 0 0 0 1.7-1.6L16 7.4"/><path d="M10.4 10.6v6.2M13.6 10.6v6.2"/>`,
  ),
} as const;

export type IconName = keyof typeof icons;
