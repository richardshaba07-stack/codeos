# Moat — RETRIEVAL precision (2026-06-14)

> The piece the scaled benchmark did NOT cover. That one injected the single relevant
> decision by hand (= it tested in-context obedience, i.e. prompting). Here we test the
> product's real MECHANISM: auto-anchoring + the context pack's segment-match filter.
> **Question: does CodeOS open the right tap (recall) without flooding the house
> (precision)?** Measured on the binary's output, **zero subagents**.

## Setup (isolated: ephemeral server, temp DB, IN-MEMORY ledger)

A synthetic repo with 6 **unconnected** modules: billing (`charge_card`), auth
(`validate_token`), report (`wilson_score`), ledger (`transfer_funds`), audit
(`log_event`), geometry (`area_circle` — **the control**). Ledger: 5 human decisions,
each tagged with the name of ONE function; geometry **without** a decision. (Ephemeral
store because neither `CODEOS_DECISIONS` nor `CODEOS_REPO` were set → **no contamination
of the real ledger**, verified.)

## Result 1 — recall + precision on SPECIFIC tags: FULL

| Goal | Expected decision | In the pack? | Distractors in the pack? |
|---|---|---|---|
| `…retry in charge_card` | D1 (payments) | ✅ only D1 | none |
| `…a single score in wilson_score` | D3 (Wilson) | ✅ only D3 | none |
| `…remember me in validate_token` | D2 (auth) | ✅ only D2 | none |
| **`…optimize area_circle` (CONTROL)** | — none — | ✅ **no WHY** | none |

**Recall 4/4** (every goal surfaces EXACTLY its decision). **Full precision on specific
tags:** zero distractor leak, and — the datum that matters most — on the **control with
no decision the pack is clean** (no `WHY`): no material injected where it isn't needed,
so no trigger for spurious refusals. The expansion stayed tight to the goal's module (it
did not pull in the others).

→ On identifier names (functions/types) the **`::` segment** match (not substring) works:
"card" does not hook "charge_card", and conceptual tags are not segments of any qualname.

## Result 2 — the honest FLAW: tag = a COMMON path segment → FLOOD

Registering D7 tagged **`src`** (a segment present in the qualified_name of EVERY
entity: `…::src::billing::charge_card`):

- goal `area_circle` (unrelated to "src") → **D7 APPEARS** ⇒ a **false positive** on the
  control (the case that, in production, can cause a spurious refusal).
- goal `charge_card` → now **D1 + D7** appear ⇒ noise that steals pack budget.

**Cause (pinned in the code):** the anti-FP guard `MAX_ANCHOR_MATCHES = 8` protects only
the **auto-anchoring** from tags that match too many entities. But the pack-time filter
matches any selected entity whose qualified_name has a `::`-segment equal to the tag
**with no cap**: a hand-supplied tag that coincides with a common segment (`src`, `tmp`,
crate names, `mod`, `lib`…) hooks EVERY selected entity and the decision enters
everywhere. `MAX_CONTEXT_DECISIONS` limits the COUNT, not the relevance. The guard exists
on one door (anchoring) but not the other (retrieval).

## Honest reading

- The moat mechanism **works** when decisions are anchored to specific identifiers (the
  normal auto-anchoring case, which picks rare leaf names): full recall, full precision,
  clean control.
- But it has a **precision hole** on low-specificity manual tags (common path segments).
  It is exactly the "flooding the house" I feared: a badly tagged decision slips into
  non-relevant packs → risk of a spurious refusal.

## Limits of this test

- **Unconnected** modules → the BFS expansion does not pressure precision (a harder test:
  billing that CALLS audit, to see if the neighbor's decision enters — but that would be
  legitimate relevance, not a false positive).
- **Synthetic** substrate; small ledger (6 decisions); a single run.
- Does not test downstream agent behavior (covered by `RISULTATI_SCALED.md`): here we
  measure ONLY what enters the pack.

## What it adds to the moat program

The scaled benchmark showed *"if the right decision is in the pack, the agent follows it"*
(18/18→0/18). This test shows *"CodeOS puts the right decision in the pack, and only that
one — EXCEPT when the tag is a common segment"*. Together they close the
injection→behavior loop and identify the **next concrete fix** (an anti-FP cap on the
retrieval filter).

---

## ⚠️ CORRECTION (2026-06-14, same day) — the "flood" is the flip side of a feature

I ATTEMPTED the fix above and WITHDREW it, because trying it made me realize the
diagnosis was incomplete. Honesty first: here is what I learned.

**1. The fix lives in TWO places, not one.** Decision selection for the pack is
DUPLICATED: in the query engine (for `codeos query`) AND in the guardian's context-pack
(for `codeos context` and the MCP `codeos_context_pack`). My first fix touched only
`query` → the live flood remained because `context` uses the guardian. (That itself is a
debt: the same anti-flood logic lives in two copies that can diverge.)

**2. The "tag = the leaf name of an entity" discriminant is WRONG.** A guardian test uses
entities `app::api::handler_i::run` and a decision tagged **`api`**: `api` is a segment
but is NOT the leaf of any entity — **exactly like `src`**. They are **structurally
identical**. The leaf-check throws out `api` (a layer the user tags on purpose, as in
`decide --boundary "api|core"`) together with `src`. That is: the "flood" is the FLIP
SIDE of an INTENTIONAL feature — anchoring a decision to ANY qualname segment, which is
what makes layer/module tags work.

**3. No cheap discriminant holds.** `≤8 absolute` → discards large modules. `fraction of
the graph` → in a single-module graph the module tag is at 100% like a path root.
`leaf-name` → discards layer namespaces (`api`). In a single-module graph, `src` and the
module name are **indistinguishable** without semantic information.

**4. The CORRECT discriminant (but non-trivial):** exclude the segments that are part of
the **prefix common to ALL entities** (the absolute path to the repo root:
`private::tmp::…::src`). `src` is in the common prefix of all; `api`/`core`/`billing` are
not (only of a subtree). It holds on all multi-module graphs — **but** it breaks
single-module graphs (there the module name IS the common prefix) and requires computing
the global common prefix (a scan of all entities). Not a 5-line fix: it touches two
sites, has a stated edge case, and must be measured.

**Decision (quality-before-speed pact):** I did NOT ship a fix that traded the flood for
a regression of layer-tagging (a red test proved it). I **reverted** both changes and
restored green. The "flood" stays a **documented limitation, not a clean bug**: it only
shows up if a human tags a decision with a path-root segment (`src`, `tmp`, the repo
name) — which nobody does on purpose (you tag `billing`, `api`, `payment`).

**Revised honest conclusion:** the moat's retrieval works well on the tags a human
actually writes (function/type/module/layer names): full recall, clean control. The
"flaw" is a pathological-tag case, and its correct fix (common-root-prefix exclusion, in
TWO sites, with a single-module edge case) is its own piece of work, to be measured — not
the hasty quick-fix I wrote above. It stays as a precise item, not a rushed patch.

---

## ✅ RESOLVED — the fix that actually shipped (recorded after the fact)

Both the "two sites" debt and the flood itself were later closed. This note exists so the
doc stops describing an open item that the code has already handled.

**1. The duplication is gone.** Decision selection now lives in **one** place,
`codeos_memory::select_human_decisions` (`crates/codeos-memory/src/selection.rs`), used by
both the query engine and the guardian context-pack. The two copies can no longer diverge.

**2. The flood is fixed with a blocklist, not common-prefix exclusion.** Rather than the
expensive global-common-prefix scan (with its single-module edge case), the shipped fix is
a small, universal blocklist of **structural** path/build segments — `src`, `lib`, `tmp`,
`tests`, `target`, `build`, `node_modules`, `vendor`, `mod`, `index`, `__init__`, … —
checked case-insensitively in `is_structural_segment()`. A tag equal to one of these no
longer anchors anything, so it cannot flood the pack; real domain/layer tags (`api`,
`core`, `billing`) are untouched because they are not in the list.

**3. Covered by tests** (`selection.rs`): the `src` flood no longer matches; a layer tag
(`api`) still anchors (no regression of the intentional feature the revert was protecting);
the filter is case-insensitive; and an explicit `related_entity_ids` anchor survives even a
structural tag.

**Honest residue (unchanged):** a tag that equals the *repository name* (not in the
blocklist) would still match broadly. This is rare and an obvious misuse — nobody tags a
decision with the repo name — so it is left as a known, documented edge rather than paying
for the global-prefix machinery. Everything a human actually tags is handled.
