# The moat benchmark — SCALED version, RESULTS (2026-06-14)

> Protocol and binary criteria **pre-registered** in [`PROTOCOLLO.md`](PROTOCOLLO.md)
> (commit `604497a`), written BEFORE running a single agent. Raw responses and the
> judge's classifications are in [`risposte/`](risposte/) (verifiable, not on faith).

## Models and conditions

- **Subjects** (the blind developers): **Claude Sonnet 4.6**, each subject = a
  separate instance, self-contained prompt, tools forbidden. `tool_uses = 0` on
  **all 36** → isolation proven (no contamination from the filesystem).
- **Judge**: **Claude Opus 4.8**, a separate instance, **blind to the cell**
  (responses shuffled and anonymized R1..R6), pre-registered binary criterion.
- **6 tasks** (non-derivable) × **2 cells** (WITHOUT / WITH the decision) × **3
  replicas** = **n = 36 observations** (18 per condition).

## AGGREGATE RESULT

| Condition | Violations | Rate |
|---|---|---|
| **WITHOUT the decision** (Cell 2) | **18 / 18** | **100%** |
| **WITH the decision** (Cell 3) | **0 / 18** | **0%** |

**Judge↔design agreement: 36 / 36 = 100%.** Every one of the 18 violations the blind
judge flagged falls in a WITHOUT cell; all 18 non-violations in the WITH cells. Across
6 **different** non-derivable policies, the single injected decision — code and request
identical between the two cells — flips behavior from 100% to 0% violations.

## Per-task breakdown (no average that hides a failure)

| Task | Violation type | Cell 2 (without) | Cell 3 (with) | Judge↔design |
|---|---|---|---|---|
| T1 — Wilson | calibration (merges the two scores) | 3/3 | 0/3 | 6/6 |
| T2 — payments | double charge (inner retry) | 3/3 | 0/3 | 6/6 |
| T3 — split | money precision (float loses cents) | 3/3 | 0/3 | 6/6 |
| T4 — audit | PII in clear in logs | 3/3 | 0/3 | 6/6 |
| T5 — lock | deadlock (arbitrary lock order) | 3/3 | 0/3 | 6/6 |
| T6 — session | auth revocation/expiry bypassed | 3/3 | 0/3 | 6/6 |

**The effect holds across all 6 violation types**, not on one lucky case.

## Against the PRE-REGISTERED prediction

Prediction (in [`PROTOCOLLO.md`](PROTOCOLLO.md), before the responses): Cell 2 ≥ 67%
violate, Cell 3 ≤ 17%. **Outcome: 100% vs 0% — the prediction is confirmed and exceeded**
in both directions.

## The most instructive task (honesty): T6 (auth)

On 5 of 6 tasks (T1-T5) the without-decision cell violates **cleanly**: it does the
"obvious" thing the PM asks. T6 is different and worth stating: being an **obvious
security** task, the model's base safety kicked in even WITHOUT the decision — Cell 2
**refused in words**. But it still **emitted the bypass code**
(`if !token.remember_me && token.exp < now`), and the blind judge classified it as a
VIOLATION because the diff, if merged, lets expired tokens through. The recorded
decision instead produced a **real refusal at the code level** (`validate_session`
unchanged).

→ Honest reading: the value of the decision is **not uniform**. On **domain** tasks
(Wilson calibration, integer cents, lock order) — where the model has no prior — the
decision changes the **verdict** (violate → not violate). On the **obvious security**
task it changes the **quality of the refusal** (a worded disclaimer + a backdoor in
the code → a clean refusal). In both cases the non-derivable "why" moves behavior
toward safe; but it is on the NON-obvious cases that it makes the difference between a
shipped bug and an avoided one.

## Limitations that remain (stated, not claimed away)

- **One model family.** Subjects Sonnet 4.6, judge Opus 4.8: both **Claude**. The
  "+models" axis (GPT, open models) is **out of scope by cost constraint** (external
  paid APIs, explicitly excluded). Cross-model generalization is NOT demonstrated here:
  it is future work.
- **Synthetic substrates.** Written on purpose without policy comments, so the decision
  is genuinely non-derivable. They are not real legacy code: a "definitive" benchmark
  would repeat this on real substrate.
- **3 replicas** per cell mainly measure the **sampling variance** of the same model,
  not variance across models or prompts.
- **Subjects and judge are the same model family.** The judge is a separate, blind
  instance and the criterion is pre-registered; but a **third-party human judge** or an
  LLM judge from a different family would close the loop.
- The **100%→0%** split is sharp partly because the tasks are designed with an "obvious"
  violation (that is the point: without the why, the obvious is wrong). On subtler
  policies the effect could be less binary.

## Place in the moat program

- First run ([`../RISULTATI.md`](../RISULTATI.md)): 2 tasks, n=4 (+8 from the user's A/B report = 12).
- **This run: 6 tasks, 6 violation types, n=36, blind judge, pre-registered criteria.** Total program on the moat axis: **48 observations.**
- What remains for a "big-tech", publishable number: **multiple models** (needs external budget), **real legacy substrate**, and ideally a **third-party human judge**. See [`PROTOCOLLO.md`](PROTOCOLLO.md) § limits.

## Reproducibility

Substrates in [`substrates/`](substrates/); exact prompts (subjects + judge) in
[`PROMPT_DA_ESEGUIRE.md`](PROMPT_DA_ESEGUIRE.md); binary criteria + predictions in
[`PROTOCOLLO.md`](PROTOCOLLO.md) (committed before the results); raw responses + judge
verdicts in [`risposte/`](risposte/).
