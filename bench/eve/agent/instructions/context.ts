import { defineDynamic, defineInstructions } from "eve/instructions";
import { bootstrap } from "../../lib/mock";

export default defineDynamic({
  events: {
    "session.started": async () => defineInstructions({ content: (await bootstrap()).instructions }),
  },
});
