# The moat benchmark — reproducible, pre-registered, blind-judged

This is the evidence behind the headline claim in the main README:

> An AI coding agent violated a **non-derivable** policy **18/18 times without**
> CodeOS's recorded intent — and **0/18 times with it.**

Nothing here is "take it on faith." The binary criteria and the protocol were
written **before** a single agent ran (commit `604497a`), the exact prompts are
included, and the **raw agent responses and the blind judge's verdicts** are in
[`scaled/risposte/`](scaled/risposte/).

> **Language.** The protocol, criteria, results and validation docs are in English.
> The benchmark was originally **run in Italian** (the author's working language); the
> exact prompts as run ([`scaled/PROMPT_DA_ESEGUIRE.md`](scaled/PROMPT_DA_ESEGUIRE.md))
> and the agents' **verbatim responses** ([`scaled/risposte/`](scaled/risposte/)) are
> kept in the original language as the authentic record — translating verbatim evidence
> would falsify it, and the Rust diffs in them (the operative proof of each violation)
> are language-neutral.

## What it actually tests

The thing git does **not** record: the *why* behind a non-obvious decision. Each
task gives an agent a small, self-contained piece of code and a plausible request.
The "obvious" implementation introduces a real defect (merges two scores that must
stay separate, double-charges on retry, loses cents to float, logs PII, deadlocks,
bypasses session expiry). The correct answer depends on a decision that **cannot
be derived from the code itself** — exactly the gap CodeOS's intent ledger fills.

## Design

- **6 non-derivable tasks × 2 cells × 3 replicas = n = 36** (18 per condition).
- The **only** difference between the two cells is one injected block. Code and
  task are byte-for-byte identical.
  - **Cell 2 (without):** stripped code + task.
  - **Cell 3 (with):** stripped code + task + the one recorded decision.
- **Subjects:** Claude Sonnet 4.6 — each a separate instance, self-contained
  prompt, **tools forbidden**. `tool_uses = 0` on all 36 → isolation proven (no
  contamination from the filesystem or this repo).
- **Judge:** Claude Opus 4.8 — a separate instance, **blind to the cell**
  (responses shuffled and anonymized), scoring against the **binary pre-registered**
  criteria.

## Result

| Condition | Violations | Rate |
|---|---|---|
| **Without** the decision (Cell 2) | **18 / 18** | **100%** |
| **With** the decision (Cell 3) | **0 / 18** | **0%** |

**Judge ↔ design agreement: 36 / 36 (100%).** Every violation the blind judge
flagged falls in a *without* cell; every clean answer in a *with* cell. The effect
holds across **all six** violation types — full per-task breakdown in
[`scaled/RISULTATI_SCALED.md`](scaled/RISULTATI_SCALED.md). It is not one lucky case.

The pre-registered prediction (written first) was: Cell 2 ≥ 67% violate, Cell 3
≤ 17%. The observed 100% vs 0% confirms and exceeds it in both directions.

## Honest limitations (stated, not hidden)

This is a real result, not a finished proof. What it does **not** show:

- **One model family.** Subjects and judge are both Claude. Cross-model
  generalization (GPT, open models) is **out of scope** here — it needs paid
  external APIs, deliberately excluded under a €0 constraint. Not demonstrated.
- **Synthetic substrates.** Written without policy comments so the decision is
  genuinely non-derivable — not real legacy code. A "definitive" benchmark would
  repeat this on real substrate.
- **3 replicas** measure the **sampling variance of one model**, not variance
  across models or prompts.
- **Same model family for subjects and judge.** The judge is a separate, blind
  instance with pre-registered criteria — but a **third-party human judge**, or an
  LLM judge from a different family, would close the loop.
- The 100→0 split is sharp **partly because** the tasks have an "obvious" wrong
  answer (that is the point: without the *why*, the obvious is wrong). On subtler
  policies the effect may be less binary.

## Reproduce it

| What | Where |
|---|---|
| Synthetic substrates (the 6 tasks) | [`scaled/substrates/`](scaled/substrates/) |
| Exact prompts (subjects + judge) | [`scaled/PROMPT_DA_ESEGUIRE.md`](scaled/PROMPT_DA_ESEGUIRE.md) |
| Pre-registered criteria + predictions (committed before results) | [`scaled/PROTOCOLLO.md`](scaled/PROTOCOLLO.md) |
| Raw agent responses + the judge's verdicts | [`scaled/risposte/`](scaled/risposte/) |
| Aggregate + per-task results | [`scaled/RISULTATI_SCALED.md`](scaled/RISULTATI_SCALED.md) |

Re-running it needs access to the two Claude models; the protocol, prompts, and
raw data are all here so anyone can audit *how* the numbers were produced, even
without re-running them.

## Also: correctness on real repositories

Separate from this efficacy benchmark, the intent-mining pipeline
(`learn` / `audit` / `certify`) was validated for **correctness** on real repos —
`gin-gonic/gin` (1,996 commits), `npryce/adr-tools`, and CodeOS itself. Highlights:
**96.5% abstention** on gin with rationale confirmed **verbatim** (character-for-character
against the real commit body), **zero fabrication**, and `certify` working on a real diff.
The run also surfaced a real improvement (write strong-signal decisions by default) that
the synthetic micro-repos had masked. Full notes:
[`REAL_WORLD_VALIDATION.md`](REAL_WORLD_VALIDATION.md).
