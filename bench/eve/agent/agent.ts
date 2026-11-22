import { createOpenAI } from "@ai-sdk/openai";
import { defineAgent } from "eve";

const provider = createOpenAI({
  baseURL: process.env.BENCH_MODEL_URL ?? "http://127.0.0.1:9100/v1",
  apiKey: process.env.BENCH_MODEL_KEY ?? "bench-token",
});

export default defineAgent({
  model: provider.responses("fake-model"),
  modelContextWindowTokens: 1_050_000,
  defaultTools: false,
  limits: { sessionTimeoutMs: false },
  experimental: { workflow: { world: "@workflow/world-postgres", modelCallsPerStep: 8 } },
});
