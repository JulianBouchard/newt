import rough from "roughjs";

export const INK = "var(--sk-ink)";
export const ACCENT = "var(--sk-accent)";

export type SketchStroke = { d: string; stroke: string; width: number; fill: string; dashed: boolean };
export type SketchLabel = {
  x: number;
  y: number;
  text: string | string[];
  size?: number;
  tone?: "ink" | "muted" | "accent";
  weight?: number;
  anchor?: "start" | "middle";
};

type Options = Record<string, unknown>;

function roundPathNumbers(d: string) {
  return d.replace(/-?\d+\.\d+/g, (number) => String(Math.round(Number(number) * 10) / 10));
}

export function createSketch(firstSeed: number) {
  const generator = rough.generator();
  const strokes: SketchStroke[] = [];
  let nextSeed = firstSeed;

  const options = (overrides: Options = {}) => ({ seed: nextSeed++, roughness: 1.1, bowing: 0.9, strokeWidth: 1.6, stroke: INK, ...overrides });

  const draw = (drawable: ReturnType<typeof generator.line>, dashed = false) => {
    for (const path of generator.toPaths(drawable)) {
      strokes.push({ d: roundPathNumbers(path.d), stroke: path.stroke, width: path.strokeWidth, fill: path.fill ?? "none", dashed });
    }
  };

  return {
    strokes,
    box: (x: number, y: number, w: number, h: number, overrides: Options = {}, dashed = false) =>
      draw(generator.rectangle(x, y, w, h, options(overrides)), dashed),
    line: (x1: number, y1: number, x2: number, y2: number, overrides: Options = {}) => draw(generator.line(x1, y1, x2, y2, options(overrides))),
    circle: (x: number, y: number, diameter: number, overrides: Options = {}) => draw(generator.circle(x, y, diameter, options(overrides))),
    ellipse: (x: number, y: number, w: number, h: number, overrides: Options = {}) => draw(generator.ellipse(x, y, w, h, options(overrides))),
    arc: (x: number, y: number, w: number, h: number, start: number, stop: number) => draw(generator.arc(x, y, w, h, start, stop, false, options())),
    path: (d: string, overrides: Options = {}) => draw(generator.path(d, options(overrides))),
    arrow: (points: [number, number][], color = INK) => {
      draw(points.length === 2 ? generator.line(...points[0], ...points[1], options({ stroke: color })) : generator.curve(points, options({ stroke: color })));
      const [fromX, fromY] = points.at(-2)!;
      const [toX, toY] = points.at(-1)!;
      const angle = Math.atan2(toY - fromY, toX - fromX);
      for (const side of [-1, 1]) {
        const headAngle = angle + Math.PI - side * 0.45;
        draw(generator.line(toX, toY, toX + 13 * Math.cos(headAngle), toY + 13 * Math.sin(headAngle), options({ stroke: color, roughness: 0.6 })));
      }
    },
  };
}
