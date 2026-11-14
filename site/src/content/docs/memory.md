---
title: Memory and compaction
description: How the agent saves facts, and how newt compacts long history into a checkpoint without dropping a commitment.
---

A session can run for months. Two things keep its history useful: the agent saves what matters, and newt compacts the rest when it grows too long.

## Saving facts

The built-in `remember` tool saves one durable fact per call ("Lives in Lisbon"). It is kept in history as an append, like any `durable` tool.

## Compaction

When live history passes `history_budget` tokens (60,000 by default, set in the bootstrap), newt compacts right after a turn commits:

1. The agent's own model gets the same prompt plus one instruction.
2. It saves anything not yet recorded.
3. It calls `checkpoint` with its memory, open threads, commitments, decisions and tone.

Nothing is sent while compacting: no outward call runs.

## Commitments cannot vanish

newt refuses a checkpoint that drops a commitment from the previous one, unless the checkpoint lists it as resolved. It also refuses an empty checkpoint, or one longer than a quarter of the budget. The agent gets the refusal as a tool result and tries again.

## What later turns see

```text
checkpoint  →  the most recent whole turns that fit in a quarter of the budget  →  anything saved during the flush
```

Raw history is never deleted. The checkpoint is a cut point, not a replacement. A compaction that fails waits before the next try (ten minutes, doubling, at most a day) and the session keeps running on its long history meanwhile.

:::note
Compaction is the one planned cache miss in a long session: it replaces the older part of history once per budget crossing.
:::

## Next

- [Design: compaction, checkpoints and commitments](../design/#compaction-checkpoints-and-commitments).
