# Use CodeOS as a guardrail for your AI coding agent (5 minutes)

AI coding agents (Claude Code, Cursor, Windsurf, Cline, Codex…) write code fast — but
they fail **silently**: a change that compiles and passes its own test, yet quietly
breaks an architectural rule, contradicts a decision made months ago, or sprawls beyond
its scope. Tests alone don't catch this (≈7.8% of "correct" agent patches introduce
regressions in *unmodified* tests).

CodeOS is a **local, deterministic, non-LLM verifier**. It plugs into your agent over
[MCP](https://modelcontextprotocol.io) so that, before and after each change, the agent
can ask *"am I breaking a governed boundary or a recorded decision?"* — and get a
**cited, abstain-by-default** answer. It never sends your code anywhere: it runs on your
machine.

> A verifier must be **separate from the model it checks** — an agent that validates
> itself with the same LLM repeats its own blind spots. CodeOS is that independent,
> deterministic check.

## What it catches

- **Boundary ignorance** — the agent adds a dependency that crosses a layer it shouldn't
  (e.g. a low-level module calling into the UI). → `codeos_guard` / `codeos_certify`
- **Stack amnesia** — the agent forgets a recorded decision (e.g. "we use Postgres, not
  Mongo") and contradicts it. → `codeos_why`, `codeos_context_pack`
- **Scope creep** — the change quietly touches modules outside its blast radius.
  → `codeos_certify` (impact + risk)

The rules it enforces come from three places, in order of authority: boundaries **mined**
automatically from your codebase's structure, **decisions** you record (`codeos decide`,
or `docs/adr/*.md`), and rules you **declare** by hand (below). You don't have to write
any config to get value — CodeOS learns your architecture from the code.

## Setup

### 1. Build

```bash
cargo build --release
# binaries: target/release/codeos  (CLI + MCP adapter) and codeos-server (graph engine)
```

### 2. (Optional) declare an explicit rule

Auto-mined and recorded rules already work. To *decree* a boundary the graph can't yet
prove, add `<repo>/.codeos/config.yaml`:

```yaml
architecture:
  rules:
    - name: "core must not depend on ui"
      type: layer_dependency
      from: ["core"]   # this layer…
      to: ["ui"]       # …must not depend on this one
```

### 3. Start the engine and index your repo

```bash
# the graph engine (keep it running); :50051 is the default address
CODEOS_REPO="$PWD" CODEOS_DB="$PWD/.codeos/graph.db" target/release/codeos-server &

# build the semantic graph
target/release/codeos index .
```

### 4. Point your agent at CodeOS

The agent launches `codeos mcp` (a thin stdio adapter that talks to the engine above).
All CodeOS tools are **read-only**, so they're safe to auto-approve.

**Claude Code**
```bash
claude mcp add codeos -e CODEOS_ADDR=127.0.0.1:50051 -- codeos mcp
```

**Cursor** — `~/.cursor/mcp.json` (or `<project>/.cursor/mcp.json`)
```json
{
  "mcpServers": {
    "codeos": { "command": "codeos", "args": ["mcp"], "env": { "CODEOS_ADDR": "127.0.0.1:50051" } }
  }
}
```

**Windsurf** — `~/.codeium/windsurf/mcp_config.json` (restart Windsurf after editing)
```json
{
  "mcpServers": {
    "codeos": { "command": "codeos", "args": ["mcp"], "env": { "CODEOS_ADDR": "127.0.0.1:50051" } }
  }
}
```

**Cline** — `cline_mcp_settings.json` (auto-approve the read-only tools)
```json
{
  "mcpServers": {
    "codeos": {
      "command": "codeos",
      "args": ["mcp"],
      "env": { "CODEOS_ADDR": "127.0.0.1:50051" },
      "autoApprove": ["codeos_guard", "codeos_certify", "codeos_why", "codeos_context_pack",
                      "codeos_query", "codeos_impact", "codeos_report", "codeos_audit",
                      "codeos_learn", "codeos_licenses"]
    }
  }
}
```

**Codex** — `~/.codex/config.toml` (or `.codex/config.toml` in a trusted folder)
```toml
[mcp_servers.codeos]
command = "codeos"
args = ["mcp"]
env_vars = { CODEOS_ADDR = "127.0.0.1:50051" }
```

## What the agent does now

At the MCP handshake CodeOS hands the agent a standing instruction: *before proposing a
change call `codeos_certify`; after editing call `codeos_guard`; check recorded decisions
with `codeos_why` / `codeos_context_pack` before touching an area.* From then on:

- **After an edit** → `codeos_guard` returns any boundary the change just crossed, with
  `file:line` and the rule it violates.
- **Before a PR** → `codeos_certify --base <ref> --head <ref>` → `✅ NO REGRESSION` /
  `⚠️ REGRESSION POSSIBLE` (exit 1), suitable as a CI gate too.

## Honest semantics (the anti-false-positive contract)

- `✅` / an empty result means **"no regression detected against known invariants"** —
  **never** "proven safe". CodeOS does not promise an absence it can't demonstrate.
- `⚠️` means a **governed boundary was crossed** — flagged with its citation, not a guess.
- Where CodeOS is unsure, it **abstains**: a missing edge is better than one that lies.
  That restraint is what makes it safe to put in front of an autonomous agent.
