# 6. SQLite (bundled) for the graph store

## Status

Accepted

## Context

The semantic graph needs to persist between runs and be queried with reasonable
performance. CodeOS is a local tool with a non-negotiable constraint: it must run on a
developer's laptop with **zero external services and zero paid APIs**. A client/server
database (Postgres, a cloud store) would break that constraint and add operational
weight no single developer wants.

## Decision

We will store the graph in **SQLite**, via `rusqlite` with the library **bundled** (no
system dependency). The store is configurable: in-memory by default (ephemeral),
persistent when `CODEOS_DB` points to a file. Access goes through a `GraphStorage`
trait so the engine never depends on the concrete database.

## Consequences

CodeOS installs and runs with a single `cargo build`, no database to provision, and
the graph file lives next to the code, versionable if desired. The `GraphStorage`
abstraction keeps the door open to another backend later. The cost is SQLite's
single-writer model, which is acceptable for an indexer that writes in batches.
