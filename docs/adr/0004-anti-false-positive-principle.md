# 4. Anti-false-positive as the core principle

## Status

Accepted

## Context

CodeOS feeds context to humans and AI agents. An agent that trusts a fabricated
relationship, a guessed "why", or an unproven invariant will make a confident wrong
decision — which is worse than having no tool at all. The whole value proposition
collapses the first time the tool lies.

## Decision

We will treat **a missing edge as better than one that lies**. Everywhere a result is
uncertain, CodeOS abstains rather than guesses: unresolved call targets stay
`Unresolved` instead of being matched by name; `learn` extracts a rationale only when
the author wrote one explicitly, copying it verbatim and citing the source; `certify`
reports "no regression *detected*", never "proven safe". Confidence is a measured
quantity (a Wilson lower bound on observed abstentions), not a heuristic.

## Consequences

Recall is sometimes lower — the tool says "I don't know" where a guesser would answer.
In exchange, what CodeOS *does* assert is trustworthy, which is the only thing that
makes it safe to put in front of an autonomous agent. Every feature is measured by its
false-positive rate, and abstention rates are reported, not hidden.
