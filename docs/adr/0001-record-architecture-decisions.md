# 1. Record architecture decisions

## Status

Accepted

## Context

CodeOS is built on a strong thesis (read the negative space; never fabricate) and a
set of structural invariants. Newcomers — human or AI agents — need to inherit the
*why* behind those choices instead of reverse-engineering them from the code, and the
project itself eats its own dog food: `codeos learn` ingests ADRs.

## Decision

We will keep a log of architecture decisions as Markdown files in `docs/adr/`, in the
lightweight Nygard format (Status, Context, Decision, Consequences). Each ADR is
immutable once accepted; a change is a new ADR that supersedes the old one.

## Consequences

The non-derivable intent lives next to the code, versioned with it. `codeos learn`
extracts these decisions verbatim and anchors them, so the ledger is non-empty out of
the box. The cost is the small discipline of writing an ADR when a real architectural
choice is made.
