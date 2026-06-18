# The moat benchmark — 2 tasks, independent judge (2026-06-13)

> This is the **first, smaller run**. It is superseded by the scaled run
> ([`scaled/RISULTATI_SCALED.md`](scaled/RISULTATI_SCALED.md), 6 tasks, n=36, blind LLM
> judge). It is kept as the record of how the result was first established.

## AGGREGATE RESULT (2 non-derivable tasks, judge blind to the design)

| Condition | Task A (Wilson) | Task B (payments) | TOTAL |
|---|---|---|---|
| **WITHOUT the decision** | 2/2 violate | 2/2 violate | **4/4 = 100%** |
| **WITH the decision** | 0/2 | 0/2 | **0/4 = 0%** |

Two independent tasks (Wilson reporting + payment retry), same structure: same code and
same task in the two cells, the only difference being the injected decision. An
independent JUDGE (a separate instance, blind to which cell was with/without the
decision) classified all 8 responses with the binary criterion: **its classification
matches the design 100%** — the 4 violations are ALL in the without-decision cells, the
4 non-violations ALL in the with cells.

→ Across 2 different non-derivable policies, the injected "why" flips behavior from 100%
to 0% violations. With the 8 agents from the user's original report: 16 total
observations on the moat axis.

## Task B — payment retry

Substrate `payments.rs`: `process_payment` calls the gateway once. The PM asks for an
automatic retry on network error. The decision forbids the retry INSIDE process_payment
(a timeout ≠ a failed charge → double charge; retries are only safe at a level that
checks state via idempotency_key).
- WITHOUT the decision: 2/2 add the retry loop inside process_payment.
- WITH the decision: 2/2 refuse, move the retry to a level with state checking.
Honest caveat: this task has a PARTIALLY derivable hint (the presence of
`idempotency_key` in the type) — the agents without the decision noticed it and added a
caveat, BUT still IMPLEMENTED the forbidden retry (a violation); with the decision they
refused. The decision moves "implement with a caveat" → "refuse". Less "pure" than the
Wilson task, stated.

## Task A — Wilson

An ISOLATED replica of Cells 2-3 that the user's original A/B report did not run "due to
a credits constraint". It closes the loop: the injected decision (the "why" that is
non-derivable from the code) changes the agent's behavior.

- **Task** (identical in the two cells): a PM asks to show ONE reliability score instead
  of two separate numbers (Wilson confidence + temporal risk) — see `reliability.rs`.
- **Cell 2** (WITHOUT): prompt = code + task. **Cell 3** (WITH): + the Wilson decision.
- 2 replicas per cell. Agents blind to the A/B, ISOLATED from the filesystem
  (`tool_uses` = 0 verified → no contamination).
- **Pre-registered BINARY criterion**: VIOLATES = the change combines/replaces the
  Wilson into a single displayed score.

| Cell | Condition | Violations | What they produced |
|---|---|---|---|
| 2 rep 1 | without | VIOLATES | `confidence * (1.0 - risk)` |
| 2 rep 2 | without | VIOLATES | `(confidence - risk).clamp(0,1)` |
| 3 rep 1 | with | does not violate | `reliability = Wilson`, risk as a label, cites the decision |
| 3 rep 2 | with | does not violate | `reliability = Wilson`, risk as separate context, cites the decision |

On the SAME request and the SAME code, the only difference — the injected decision —
flips behavior. **This is the non-derivable value of the ledger, measured under isolated
conditions.**

## A flawed first attempt (stated, not hidden)

The FIRST attempt was contaminated: the agents ran inside the project folder and 3/4
READ the real ledger and memory on disk — including Cell 2, which was NOT supposed to
have them. Invalid result. Redone with explicit isolation (no filesystem),
`tool_uses`=0. A harness bug — not a product bug — found and stated, not spun as a win.

## Honest limits

- **Small n** (4 agents; with the 8 from the original report: 12 total).
- The agents are **Claude** instances (same model).
- **A single task** (Wilson), one violation type.
- **Judge = the benchmark author**, but with a criterion pre-registered before the
  responses.
- **Synthetic** substrate, chosen so the decision is genuinely non-derivable.

For a "big-tech" benchmark you would need: more non-derivable tasks, multiple models, an
independent judge, n≥30 — which is exactly what `scaled/` does.
