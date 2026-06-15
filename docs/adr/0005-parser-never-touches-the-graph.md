# 5. The parser never touches the graph

## Status

Accepted

## Context

Source files are parsed per-file with Tree-sitter, but a global semantic graph needs
stable identities that span files (the same function referenced from many places must
resolve to one node). If the parser invented graph identities, every file would mint
conflicting ids and cross-file resolution would be impossible.

## Decision

We will keep a hard separation: the **parser only mints raw, per-file data** and never
touches the graph. Global `EntityId`s are born exclusively in the `GraphResolver`,
which performs name resolution across the whole batch and the persisted store. This is
invariant 1.4.

## Consequences

Identity is assigned in exactly one place, so cross-file references resolve
deterministically and the parser stays a pure, easily-tested transformation. The cost
is that the resolver is the sequential bottleneck of indexing on large monorepos —
a known trade-off to be optimized behind this boundary, not by leaking ids upward.
