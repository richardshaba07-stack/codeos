# Correctness validation on REAL code — `learn` / `audit` / `certify`

> Honesty: this is **not** a blind-judge benchmark. It is a **correctness validation**
> of the pipeline commands (miner + audit + certifier) on a real repository, after they
> had only been tried on synthetic micro-repos. Date: 2026-06-15.

## Repo under test

**`gin-gonic/gin`** (cloned clean from GitHub): 99 Go files, **1,996 real commits**,
13 MB. Chosen because it has a real, long history and a supported language; it is NOT
a monorepo (99 files), so it does not stress the resolver.

Ephemeral server on a high port (temporary DB). The production port was never touched.

## What was measured

### Indexing (the resolver)
- **0.3 s for 99 files** (release). At this scale the resolver is not the wall-clock
  bottleneck — it only is on monorepos (thousands of files), as already documented.
  *This repo is not meant to study resolver perf; it is meant for correctness.*

### `learn` on 1,996 real commits
- **1,780 commits scanned** (merges excluded), **63 with explicit intent**, **1,717
  abstained** → **abstention 96.5%.** The anti-FP thesis holds on real data: the vast
  majority of commits are NOT forced into a decision.
- **0 strong, 63 causal, 0 ADR.** Gin **uses no markers** (`DECISION:`/`BREAKING
  CHANGE:`) and has no `docs/adr` → the strong and ADR tiers don't fire. That is an
  honest fact about the repo, not a flaw in the miner.
- **VERBATIM rationale confirmed**: the extracted rationales match the real commit body
  character-for-character (verified on `e292e5caa…`). **Zero invention** — the anti-FP
  core holds on real code.

### Finding (the synthetic tests had masked it)
The **causal tier is noisy on real code.** Examples extracted from gin:
- ✅ a true why: *"…panics because it accesses children[0]… but addChild() keeps the
  wildcard child at the end of the array"* — a genuine technical decision.
- ⚠️ noise: *"document and finalize Gin v1.12.0 release … announce Gin 1.12.0 instead of
  1.11.0"* — a release note, hooked by the connective "instead of". Verbatim, but not a
  decision.

→ **Action taken:** `learn --write` now persists **only the STRONG signals** by default
(markers + ADR); the causal tier stays in the dry run for review and is written with
`--include-causal`. Verified on gin: `--write` → 0 written + 63 causal held back;
`--write --include-causal` → 63 written. The two-tier design is **vindicated** by the
data: separating strong from causal matters.

### `certify` on a real diff
- `certify --base HEAD~10 --head HEAD` → **✅ NO REGRESSION** (10 dependencies from the
  changed code, 0 architectural violations), MEDIUM risk, **exit 0.** The gate works on
  a real diff, not just a synthetic one.

### `audit`
- Empty ledger (gin has none) → **"Empty ledger: nothing to verify"**, exit 0. Honest
  behavior.

### ADR tier on real data — `npryce/adr-tools`
Repo with 9 authentic Nygard ADRs in `doc/adr/`. `learn` extracts **9 of 9**: title
cleaned of the numbering (`0002-implement-as-shell-scripts` → "Implement as shell
scripts"), rationale = the `## Decision` section **verbatim** (confirmed
character-for-character). The ADR tier works on real files.

**Finding (the synthetic tests had masked it):** the commit-level `ADR-N` detection
over-triggers on ADR **maintenance** commits — e.g. "Fix typo and add more consequences
to ADR 5" was extracted as a decision (ADR marker) because the subject mentions "ADR 5".
That is a commit that *edits* the ADR, not a decision.

→ **Action taken:** `learn` suppresses the ADR signal at commit level when the commit
**touches an ADR file** (the ADR is already ingested from the authoritative source). A
commit that *cites* an ADR without editing it stays valid. Measured: on adr-tools the
noise commit disappears (6→5 decision commits), the 9 ADR files remain.

## Conclusion

The whole session pipeline (mine → write → verify → certify) **runs correctly on a real
1,996-commit repo**, the anti-FP holds (96.5% abstention, verbatim rationale), and the
real-code test produced a **real improvement** (write strong-by-default) that the
micro-repos would not have revealed.

### STRONG tier on real data — `codeos` (this very repo)
Gin has no markers, so the strong tier was validated on CodeOS's own repo, which has
them: **160 commits, 6 strong, 154 abstained.** The 6 are real and verbatim — `WHY:`
markers and `ADR` references. Example (a WHY-marker commit): *"…with a goal that
localizes nothing… 'low' here does not mean 'safe', it means 'I found nothing to
evaluate' — and this pack goes straight to an AI, which would read 'low risk' and
proceed calmly."* — a complete architectural decision, extracted intact. **The strong
tier (the `--write` default) is thus validated on real data**, and it is the
high-precision tier that gin could not exercise.

## Stated limits
- **Three repos** (gin: Go/causal; codeos: Rust/strong; adr-tools: ADR), no monorepo.
- **Resolver perf not studied** (99 files is too few): a real monorepo (thousands of
  files) is needed to measure the wall-clock bottleneck.
- No judge, no pre-registered criterion: this is **correctness** on real code, not "the
  proof" with a blind judge (that is `scaled/RISULTATI_SCALED.md`).
