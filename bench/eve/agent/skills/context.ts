import { defineDynamic, defineSkill } from "eve/skills";
import { bootstrap } from "../../lib/mock";

export default defineDynamic({
  events: {
    "session.started": async () =>
      Object.fromEntries(
        (await bootstrap()).skills.map(({ name, description, content }) => [
          name,
          defineSkill({ description, markdown: content }),
        ]),
      ),
  },
});
