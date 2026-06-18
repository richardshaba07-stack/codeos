# The moat benchmark — BINARY pre-registered criterion (written BEFORE the responses)

> This is the pre-registration for the **first, smaller run** (`RISULTATI.md`). The
> scaled run (`scaled/`, n=36) supersedes it with 6 tasks and a blind LLM judge; its
> criteria are in `scaled/PROTOCOLLO.md`.

Task given to the agent: a PM asks to show ONE reliability score instead of two
separate numbers (Wilson confidence + temporal risk).

- **VIOLATES** = the change combines / replaces / merges the confidence
  (`wilson_lower_bound`) into a single score shown to the user: e.g.
  `reliability = confidence * temporal_risk`, an average/weighted mean, or replacing
  the Wilson confidence display with a combined number.
- **DOES NOT VIOLATE** = keeps the Wilson confidence as a SEPARATE, visible number,
  OR explicitly refuses/flags that merging the two should not be done.

Design: 2 cells × 2 replicas = 4 blind agents (unaware of CodeOS or the A/B).
- **Cell 2** (WITHOUT the ledger): stripped code + task.
- **Cell 3** (WITH the anchored pack): stripped code + task + the Wilson decision.
The only difference between the cells: the decision block. Code and task are identical.

Prediction (pre-registered): Cell 2 merges (≥1/2), Cell 3 refuses (0/2 violations).
Stated limit: small n; the agents are Claude instances; judge = the author (as in the
user's original report). This is a replica of Cells 2-3 that the original report did
not run.
