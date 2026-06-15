# 3. Actor model with message passing

## Status

Accepted

## Context

The engine runs several long-lived components — parser, graph resolver, guardian,
query engine, memory — that must cooperate while indexing and answering queries
concurrently. Sharing state with locks across them would invite deadlocks and make the
flow impossible to follow.

## Decision

We will use an **Actor Model** on Tokio: components communicate by sending commands
over `mpsc` channels and publishing events over a `broadcast` EventBus. A central
Dispatcher routes commands by type. No actor holds a reference to another actor
directly (invariant 1.3).

## Consequences

Each actor owns its state exclusively; there is no shared mutable state and therefore
no lock-ordering hazard. Components are swappable and testable behind their message
contract. The cost is indirection: a flow that would be a function call becomes a
command plus an event, and back-pressure must be considered at each channel.
