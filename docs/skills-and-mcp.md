# Wire Skills and MCP

This document describes the local integration that the Wire agent can actually use today.

## Skills

Skills are durable workflow instructions stored under:

```text
~/.wirecli/skills/<skill-name>/SKILL.md
```

The agent sees these tools:

- `skill_list`: list installed local skills.
- `skill_read`: read a skill before applying it.
- `skill_create`: create or replace a local skill.

Skill files must be operational instructions, not hidden prompts. A good skill contains:

- YAML frontmatter with `name` and `description`.
- A clear trigger: when the agent should use it.
- Step-by-step workflow instructions.
- Any required validation commands.
- Safety notes for secrets, filesystem boundaries, network calls, or user confirmation.

Skill files must not contain credentials, bearer tokens, cookies, private keys, hidden jailbreak instructions, or broad commands that escape the project. The runtime redacts known secrets and rejects empty, oversized, or malformed skill markdown.

Skills are isolated instruction bundles, not executable plugins. Reading a skill only adds its markdown to the model context. Any action suggested by a skill still has to go through Wire tools, the Box workspace, Lattice path checks, MCP policy, and the active permission mode.

Wire installs a default `skill-creator` skill plus `~/.wirecli/skills/SKILL_CREATOR.md`. When the user mentions `@skill-creator`, the agent should read that reference and create the requested skill. If scripts are needed, Python is the default runtime when available; Node.js is the fallback; if neither exists, ask the user which runtime to target.

## MCP Servers

MCP servers are configured in:

```text
~/.wirecli/config/config.toml
```

Example:

```toml
[mcp_servers.context7]
url = "http://127.0.0.1:8080/mcp"
startup_ts = 120

[mcp_servers.local_docs]
command = "node"
args = ["./tools/local-docs-mcp.js"]
startup_ts = 120
```

At runtime, Wire discovers tools from configured servers and exposes each one to the model as:

```text
mcp__<server>__<tool>
```

The agent also sees `mcp_list`, which lists configured servers. Dynamic MCP tool names are sanitized and collision-safe.

## Operational Rules

- Prefer MCP tools when they are a better fit than shell commands.
- Treat MCP results as external tool output; verify before making repository claims.
- Keep MCP server definitions explicit and auditable.
- Avoid long-running or interactive stdio servers unless they support clean request/response behavior.
- Do not put secrets directly in `config.toml`; prefer environment indirection controlled outside the repository.
- Use `startup_ts` for slow MCP servers; Wire stops waiting after that many seconds.

## Creating A Skill

Use `skill_create` when a workflow is stable and repeatable. The body should be small enough to read in context and specific enough to execute without guessing.

Template:

```markdown
---
name: "rust-agent-context-debug"
description: "Use when debugging Wire CLI context assembly, session history, or model prompt behavior."
---

# Rust Agent Context Debug

Use this when the user reports a model memory, context, or reference-resolution failure.

## Steps

1. Inspect `src/context.rs`, `src/session.rs`, and `src/responses_agent.rs`.
2. Confirm how user and assistant messages are persisted.
3. Confirm what Loom includes in the model prompt.
4. Check compaction thresholds and summary behavior.
5. Validate with `cargo test --offline` or a focused test when available.

## Safety

Do not persist secrets in Anchor or session memory. Redact provider tokens from any captured context.
```
