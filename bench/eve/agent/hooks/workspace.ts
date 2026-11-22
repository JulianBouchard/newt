import { defineHook } from "eve/hooks";
import { startPages } from "../../lib/mock";

export default defineHook({
  events: {
    "turn.started": (_event, ctx) => startPages(ctx),
  },
});
