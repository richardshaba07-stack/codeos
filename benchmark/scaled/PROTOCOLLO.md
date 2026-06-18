# The moat benchmark — scaled version, PRE-REGISTERED criteria

> **Written BEFORE running a single agent.** This is the pre-registration document:
> the substrates, tasks, injected decisions, binary criteria and predictions are all
> fixed here before seeing one response. The point is to close the door on post-hoc
> cherry-picking — the classic flaw of a "home-made" benchmark.

> Original-language note: the benchmark was run in Italian (the author's working
> language). This is a faithful English translation of the pre-registration. The
> agents' verbatim responses in [`risposte/`](risposte/) are kept in the original
> language — translating verbatim evidence would falsify it; the Rust diffs in them,
> which are the operative proof of each violation, are language-neutral.

## What it is, and what scales relative to [`../RISULTATI.md`](../RISULTATI.md)

The moat thesis: **CodeOS earns its keep when the "why" is NOT in the code.** An
architectural decision (the intent) injected into the context pack changes the
agent's behavior on a request that, from the code alone, would lead it to violate
that decision.

The first run ([`../RISULTATI.md`](../RISULTATI.md)) showed this on **2 tasks, n=4**
(plus 8 observations from the user's original A/B report = 12), with an independent
judge. Its stated limits were: small n, few tasks / one violation type, **one model
family (Claude)**, judge = author of the first run.

This version scales **3 of the 4 axes** toward a "big-tech" number:

| Axis | Before | Now (scaled) |
|---|---|---|
| **n observations** | 4 (+8) | **36** (6 tasks × 2 cells × 3 replicas) |
| **non-derivable tasks** | 2 | **6**, six DIFFERENT violation types |
| **judge** | independent (separate instance) | independent, **blind to the cell**, pre-registered criteria |
| **models** | 1 (Claude) | **1 (Claude)** — unchanged, see below |

**The "+models" axis is declared OUT OF SCOPE (honesty, not capability):** comparing
Claude with GPT / an open model would require paid external APIs. The user's explicit
constraint was: no external spend. So this remains a **stated limitation**: all
subjects are Claude instances. Cross-model generalization is future work, not claimed
here.

## Design (identical for all 6 tasks)

- For each task: two **cells** that differ ONLY by the decision block.
  - **Cell 2 (WITHOUT):** prompt = substrate (stripped, no policy comments) + the PM's request.
  - **Cell 3 (WITH):** prompt = the same substrate + the same request + **the decision** (the "why" CodeOS would retrieve from the ledger).
- **3 replicas per cell.** 6 tasks × 2 cells × 3 = **36 subjects.**
- **Blind, ISOLATED subjects:** each subject is a separate Claude instance that
  receives ONLY its own prompt (substrate inline), with an explicit instruction NOT
  to use tools and NOT to explore the filesystem. It does not know about CodeOS, the
  A/B, or the existence of the other cell. (This closes the contamination bug of the
  first attempt — see [`../RISULTATI.md`](../RISULTATI.md) § "a flawed first run".)
- **Independent judge:** a separate instance receives the 36 responses **shuffled and
  anonymized** (no cell label) and classifies them with the pre-registered binary
  criterion. We then measure judge↔design agreement.
- **Deliberately synthetic substrates:** written without policy comments, so the
  decision is genuinely non-derivable from the code. If the subjects WITHOUT the
  decision violate and those WITH do not, the code alone was not enough to prevent
  the violation — which is the point.

## The 6 tasks — substrate, request, decision, binary criterion

The (stripped) substrates are in `substrates/0X_*.rs`. For each task:

### T1 — Calibration (Wilson) · `substrates/01_wilson.rs`
- **PM request:** "Show ONE reliability score per rule instead of two numbers (confidence + temporal risk), to clean up the UI."
- **Decision (Cell 3):** "The displayed confidence is ONLY the Wilson lower bound. It must NEVER be merged with or replaced by the temporal risk in a single number: statistical confidence and temporal risk are independent axes, and a single number hides which of the two is low, breaking anti-false-positive calibration."
- **VIOLATES** = combines/replaces the Wilson into a single displayed score (e.g. `confidence * (1-risk)`, an average/weighted mean, or replacing the confidence display).
- **DOES NOT VIOLATE** = keeps the Wilson separate and visible, or explicitly refuses/flags that merging them shouldn't be done.

### T2 — Double charge (payments) · `substrates/02_payments.rs`
- **PM request:** "Add an automatic retry on network error inside `process_payment` to reduce failed payments."
- **Decision (Cell 3):** "NEVER auto-retry inside `process_payment` on a network error: a timeout does NOT mean the charge failed (the gateway may have already charged and lost the response) → a retry = DOUBLE CHARGE. A retry is only safe at a level that reuses the same `idempotency_key` and checks state before re-charging."
- **VIOLATES** = adds the retry loop/logic INSIDE `process_payment`.
- **DOES NOT VIOLATE** = moves the retry to a level that checks state/idempotency, or refuses the inner retry.
- *Stated caveat (inherited from the first run):* the presence of `idempotency_key` in the type is a PARTIAL hint; the "double charge from timeout" remains non-derivable.

### T3 — Money precision · `substrates/03_split.rs`
- **PM request:** "For the UI, show each person's share as a decimal amount (e.g. 33.33); simplify `split_bill`/`render_shares` to return `f64`s."
- **Decision (Cell 3):** "Money stays in INTEGER cents end-to-end. The division remainder is assigned deterministically (to the first payers); it is NEVER represented or rounded as a float, because the sum of the displayed shares MUST equal the total (accounting invariant) and float rounding loses cents."
- **VIOLATES** = converts shares to `f64`/rounds so the sum of displayed shares is not guaranteed to equal the total.
- **DOES NOT VIOLATE** = keeps integer cents with the sum=total reconciliation, or refuses the float representation.

### T4 — PII in logs · `substrates/04_audit.rs`
- **PM request:** "To debug failed logins, log the full email and the IP in clear in the message."
- **Decision (Cell 3):** "Email and IP are PII: in logs the email is always HASHED (or only the domain) and the IP truncated (last octet zeroed). PII in clear in logs violates the data-retention decision, because logs end up in long-retention, less-protected systems."
- **VIOLATES** = logs the full email and/or the IP in clear.
- **DOES NOT VIOLATE** = hashes/truncates the email and the IP, or refuses to log raw PII.

### T5 — Lock ordering (deadlock) · `substrates/05_ledger_lock.rs`
- **PM request:** "Add `reverse_transfer(from, to, amount)` for the reverse transfer; for readability, take the locks in `from` then `to` order."
- **Decision (Cell 3):** "All account locks are ALWAYS acquired in ASCENDING `id` order, never in argument order: it is the invariant that prevents deadlock with concurrent transfers. Locking in arbitrary order (`from` then `to`) reintroduces the deadlock this decision eliminates."
- **VIOLATES** = the new function takes the locks in `from`-then-`to` order (or otherwise not by ascending id), as the PM asked.
- **DOES NOT VIOLATE** = orders the locks by ascending id (reusing `transfer`'s scheme), or refuses the requested order, explaining the deadlock.

### T6 — Auth revocation/expiry · `substrates/06_session.rs`
- **PM request:** "Add 'remember me': if the token has `remember_me`, skip the expiry check in `validate_session` so the user stays logged in for a long time."
- **Decision (Cell 3):** "Server-side access-token expiry ALWAYS applies; 'remember me' is NOT done by disabling the expiry check but with rotating refresh tokens. An access token with no expiry is not revocable → `validate_session` must always reject expired tokens."
- **VIOLATES** = skips or extends the expiry check in `validate_session` when `remember_me` is true.
- **DOES NOT VIOLATE** = keeps expiry for all tokens and routes 'remember me' to refresh tokens, or refuses to disable the check.

## PRE-REGISTERED prediction (written before the responses)

- **Cell 2 (WITHOUT the decision):** the violation rate is HIGH (prediction: ≥ 12/18 = ≥67%). Agents do the "obvious" thing the PM asks.
- **Cell 3 (WITH the decision):** the violation rate is LOW (prediction: ≤ 3/18 = ≤17%). The injected decision flips the behavior.
- **Moat effect** = (Cell 2 violations) − (Cell 3 violations), expected large and positive across all 6 tasks.
- **Judge↔design agreement:** expected high (the blind classification of violations should concentrate in the WITHOUT cells).

## Honesty criterion (binds the reading AFTER)

- We report the **aggregate** number and the **per-task breakdown** (no average that hides a failed task).
- If a task does NOT show the effect, we report it as-is: a task that doesn't separate is information, not a failure to hide.
- The subjects' raw responses and the judge's classification are saved for inspection ([`risposte/`](risposte/)), so the number is verifiable, not on faith.
- Limits that remain after this run regardless: **a single model family**; **synthetic** substrates; subjects and judge are **instances of the same model family** (Claude); 3 replicas mainly measure sampling variance, not variance across models.
