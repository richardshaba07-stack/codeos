# 2. Onion-layered crate architecture

## Status

Accepted

## Context

CodeOS is a Cargo workspace of ten crates (types, storage, parser, graph, memory,
paleo, guardian, query, core, rpc). Without a dependency rule, crates tend to grow
cyclic dependencies, which make the build fragile, the tests slow, and reasoning about
ownership impossible.

## Decision

We will enforce an **onion** dependency order: each crate may depend only on the crates
declared before it. `codeos-types` (the data model and the event/command bus) sits at
the center and depends on nothing internal; every other crate forms a strictly outer
layer. This is invariant 1.5 of the project.

## Consequences

The dependency graph is a DAG by construction — no cycles, predictable build order,
and each layer is testable in isolation. CodeOS can mine its own layering invariants
from this asymmetry. The cost is that a lower layer can never reach "up": shared logic
must live at or below the layer that needs it, which occasionally forces an extraction.
