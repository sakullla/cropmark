import type { CatalogKey } from "../i18n";
import type { AnnotationTool } from "../annotation";

export interface RegionTools {
  arrow: boolean;
  rect: boolean;
  ellipse: boolean;
  highlighter: boolean;
  mosaic: boolean;
  text: boolean;
  number: boolean;
  spotlight: boolean;
  magnifier: boolean;
  bubble: boolean;
  sticker: boolean;
  erase: boolean;
  line: boolean;
  blur: boolean;
  pin: boolean;
  ocr: boolean;
  qr: boolean;
}

export type RegionToolId = AnnotationTool | "line" | "blur" | "pin" | "ocr" | "qr";

export const REGION_TOOL_FIELDS: { id: RegionToolId; labelKey: CatalogKey }[] = [
  { id: "arrow", labelKey: "selection.tool.arrow" },
  { id: "rect", labelKey: "selection.tool.rect" },
  { id: "ellipse", labelKey: "selection.tool.ellipse" },
  { id: "highlighter", labelKey: "selection.tool.highlighter" },
  { id: "mosaic", labelKey: "selection.tool.mosaic" },
  { id: "text", labelKey: "selection.tool.text" },
  { id: "number", labelKey: "selection.tool.number" },
  { id: "spotlight", labelKey: "selection.tool.spotlight" },
  { id: "magnifier", labelKey: "selection.tool.magnifier" },
  { id: "bubble", labelKey: "selection.tool.bubble" },
  { id: "sticker", labelKey: "selection.tool.sticker" },
  { id: "erase", labelKey: "selection.tool.erase" },
  { id: "ocr", labelKey: "selection.action.ocr" },
  { id: "qr", labelKey: "selection.action.qr" },
];

