import { readFileSync } from "node:fs";
import { join } from "node:path";
import opentype from "opentype.js";
import type { SketchLabel } from "./rough-sketch";

const CAVEAT_WEIGHTS = [500, 600, 700] as const;

const fonts = new Map(
  CAVEAT_WEIGHTS.map((weight) => {
    const file = readFileSync(join(process.cwd(), `node_modules/@fontsource/caveat/files/caveat-latin-${weight}-normal.woff`));
    return [weight, opentype.parse(file.buffer.slice(file.byteOffset, file.byteOffset + file.byteLength))] as const;
  }),
);

function closestWeight(weight: number) {
  return CAVEAT_WEIGHTS.reduce((best, candidate) => (Math.abs(candidate - weight) < Math.abs(best - weight) ? candidate : best));
}

export type GlyphDefinition = { id: string; d: string };
export type GlyphLine = { transform: string; tone: NonNullable<SketchLabel["tone"]>; glyphs: { href: string; x: number }[] };

const GLYPH_UNITS = 100;

function joinCoordinates(values: number[]) {
  return values.map((value, index) => (index > 0 && value >= 0 ? ` ${value}` : `${value}`)).join("");
}

function glyphPathData(glyph: opentype.Glyph) {
  return glyph
    .getPath(0, 0, GLYPH_UNITS)
    .commands.map((command) => {
      if (command.type === "Z") return "Z";
      const points =
        command.type === "C"
          ? [command.x1, command.y1, command.x2, command.y2, command.x, command.y]
          : command.type === "Q"
            ? [command.x1, command.y1, command.x, command.y]
            : [command.x, command.y];
      return command.type + joinCoordinates(points.map((value) => Math.round(value)));
    })
    .join("");
}
const round = (value: number, places = 1) => Math.round(value * 10 ** places) / 10 ** places;

export function labelsToGlyphs(labels: SketchLabel[], idPrefix: string) {
  const definitions = new Map<string, GlyphDefinition>();
  const lines: GlyphLine[] = [];

  for (const label of labels) {
    const size = label.size ?? 20;
    const weight = closestWeight(label.weight ?? 500);
    const font = fonts.get(weight)!;
    const texts = Array.isArray(label.text) ? label.text : [label.text];
    texts.forEach((text, lineIndex) => {
      const width = font.getAdvanceWidth(text, size);
      const startX = label.anchor === "middle" ? label.x - width / 2 : label.x;
      const baseline = label.y + lineIndex * size * 1.05;
      const toUnits = GLYPH_UNITS / size;
      const glyphs: GlyphLine["glyphs"] = [];
      font.forEachGlyph(text, startX, baseline, size, {}, (glyph, glyphX) => {
        if (!glyph.path.commands.length) return;
        const id = `${idPrefix}${weight / 100}-${glyph.index}`;
        if (!definitions.has(id)) definitions.set(id, { id, d: glyphPathData(glyph) });
        glyphs.push({ href: `#${id}`, x: round((glyphX - startX) * toUnits) });
      });
      lines.push({ transform: `translate(${round(startX)} ${round(baseline)}) scale(${round(size / GLYPH_UNITS, 3)})`, tone: label.tone ?? "ink", glyphs });
    });
  }
  return { definitions: [...definitions.values()], lines };
}
