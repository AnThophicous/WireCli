# Wire CLI

`wirecli` is the user-facing binary and the public project identity.

Wire CLI is a local-first CLI developed by AnThophicous and contributors.

## Install

For the npm release, the expected command is the compiled global CLI:

```bash
npm install -g wirecli
wirecli
```

Global npm installation is required if you want to launch it directly as `wirecli` instead of `cargo run -- wirecli`.
The published `wirecli` package is only the launcher. npm automatically pulls the matching prebuilt platform package for the user's OS/CPU, so the end user does not need Rust or Cargo just to install and run it.

Current release targets:

- Linux x64 -> `@wirecli/linux-x64`
- Windows x64 -> `@wirecli/win32-x64`

## Publishing

The npm release is split into three packages:

- `@wirecli/linux-x64` -> compiled Linux binary
- `@wirecli/win32-x64` -> compiled Windows binary
- `wirecli` -> thin launcher that depends on the correct platform package

The release workflow in `.github/workflows/release-npm.yml` builds Linux on Ubuntu and Windows on `windows-latest`, stages the binaries into the platform packages, publishes the platform packages first, and only then publishes the root `wirecli` package.

The Wire CLI state lives under the current user's home directory in `~/.wirecli/`.
Project-local `.wirecli/` directories are legacy/local artifacts only; when Wire sees one in a Git project, it adds `.wirecli/` to `.gitignore`.

## Core layers

- **Box**: the writable project workspace the agent acts on.
- **Lattice**: the execution perimeter around the Box.
- **Anchor**: durable memory stored in bundled SQLite through Rust `rusqlite`.
- **Tide**: the live session history and event stream.
- **Loom**: the context builder that assembles the prompt for each model turn.
- **Providers**: OpenRouter by default, plus API-key providers configured in user config.

## Storage

- `~/.wirecli/config/config.toml`
- `~/.wirecli/config/config.md`
- `~/.wirecli/config/secret.key`
- `~/.wirecli/data/history.sqlite3`
- `~/.wirecli/data/anchor.sqlite3`
- `~/.wirecli/data/memory_context.json`
- `~/.wirecli/data/harness/runs/`
- `~/.wirecli/data/approvals.json`
- `~/.wirecli/data/approval_audit.jsonl`
- `~/.wirecli/skills/`
- `~/.wirecli/hooks.json`
- `~/.wirecli/boxes/`

## Commands

- `wirecli`
- `tui`
- `login`
- `run <prompt...>`
- `models`
- `providers [provider-id]`
- `approvals [list|allow-once|allow-repo|deny|deny-always]`
- `hooks [list|add|remove]`
- `skills [list|read|create]`
- `harness [run|replay|inspect|doctor|evals]`
- `box [new|list|run|tools]`

`wirecli` opens the full-screen TUI by default.
Each `wirecli` launch starts on a fresh session. Use `wirecli sessions` or `wirecli resume [session-id]` to continue an older project session.

## Harness

`wirecli harness` wraps the native Wire agent loop with an auditable run plane:

- `wirecli harness run --prompt "..."` emits NDJSON events and stores the run log.
- `wirecli harness run --prompt-file task.md --text` streams plain assistant text while still saving NDJSON.
- `wirecli harness inspect latest` prints the latest run metadata and verification result.
- `wirecli harness replay latest` replays the stored NDJSON stream.
- `wirecli harness doctor` checks provider, model, tool, MCP, git, and run-log state.
- `wirecli harness evals` prints the internal regression benchmark catalog.

Harness runs include the exposed built-in tool manifest, MCP discovery results, preflight context, tool start/end events with durations, token usage, deterministic post-run verification, ACP/WDF recovery checkpoints, and a private metadata sidecar under `~/.wirecli/data/harness/runs/`.

## Providers

OpenRouter is the default provider and uses the integrated PKCE flow:

```bash
wirecli login
```

API-key providers are configured with env vars or the user config:

- `openai` -> `OPENAI_API_KEY`
- `deepseek` -> `DEEPSEEK_API_KEY`
- `anthropic` / Claude -> `ANTHROPIC_API_KEY`
- `zai` / GLM-5.1 -> `ZAI_API_KEY`
- `google` / Gemini -> `GEMINI_API_KEY`
- `qwen` -> `DASHSCOPE_API_KEY`
- `mistral` -> `MISTRAL_API_KEY`
- `xai` -> `XAI_API_KEY`

Use `wirecli providers` to list presets and `wirecli providers deepseek` to apply one.

Custom providers can be configured by editing `~/.wirecli/config/config.toml`.
Wire CLI does not use `better-sqlite3` or any Node-native database module for end-user storage.

```toml
model_provider = "meu-router"
model = "router-smart"
approvals_reviewer = "user"
model_reasoning_effort = "medium" # optional, only for compatible models

[model_provider.meu-router]
name = "Meu Router"
base-url = "http://localhost:3000/v1"
method = "completions" # completions or responses
env_key = "MEU_ROUTER_API_KEY"
models = ["router-fast", "router-smart", "router-coder"]

[model_provider.meu-router.models.reasoning]
model = "router-reasoning"

[features]
memories = true
auto-context-compact = true
terminal_resize_reflow = true
image_generation = false

[feature_context]
enabled = true
afup = true
flash_cache_memory = true
automatic_context_compaction = true
acc_model = "openrouter/free"
fcm_max_entries = 192

[mcp_servers.nome-do-server]
url = "https://example.com/mcp"
startup_ts = 120

[mcp_servers.outro-server]
command = "npx"
args = ["-y", "aqui-o-server"]
startup_ts = 120
```

`[model-provider.nome]`, `[model_provider.nome]`, and `[model_providers.nome]` are accepted. Prefer `models = ["a", "b"]` for multiple models; TOML does not support repeating `model = ""` several times in the same table.

## Memory tools

- `remember` stores durable Anchor memory.
- `recall` looks up relevant Anchor memory.
- `WIRE.md` is the preferred project memory file. `AGENTS.md` remains supported for compatibility. Rules written as `path: src/** -> ...`, `type: *.rs -> ...`, or `global: * -> ...` are matched into the active context when relevant.
- `AFUP` means Adaptive Framework for User Patterns. It learns durable user patterns through the `lab_learn`/`lab_recall` path and adapts style, tooling, validation habits, and workflow choices without treating preferences as proof of external facts.
- `ACC` means Automatic Context Compaction. It keeps long sessions resumable and uses `[feature_context].acc_model` for the compacting model, so the main coding model does not have to spend the entire budget summarizing itself.
- `FCM` means Flash Cache Memory. Wire creates a project-local `.wci/mm.fcm` cache with fast recontextualization signals, project scan hints, and ACC summaries. `.wci/` is gitignored automatically because it is local runtime state.
- MemoryContext supports ranking, path scopes, access count, and expiration for temporary facts. Wire can surface `memory.suggestion` as "Aprendi isso, quer salvar?" without silently writing durable memory.
- Wire can surface `skill.suggestion` when a repeated workflow looks reusable; confirm the workflow before turning it into a local `SKILL.md`.
- `hooks add <event> [filters] -- <command...>` attaches an automatic lifecycle command. Events include `session_start`, `pre_tool_use`, `post_tool_use`, `file_changed`, `pre_compact`, `stop`, `stop_failure`, `permission_request`, `after_shell`, `after_edit`, and `after_commit`.
- Hook filters: `--match-tool <name>`, `--match-status <status>`, `--match-path <path>`, `--match-command <command>`, and `--match-mode exact|starts_with|contains`.

## Permission modes

- `Normal`: edit files and run allowed development commands only inside the project Box. Risky commands create approval requests before they run.
- `Guardian`: stay inside the Box and ask the configured provider to review commands after Lattice and local approval checks.
- `Full Access`: not recommended; unrestricted host filesystem, command, and network access.

## Sandbox and approvals

On Linux, command tools run through `bubblewrap` with network disabled by default, a private `/tmp`, isolated home, PID/user/network namespaces, process-tree timeout handling, and CPU/memory/process limits. If `bubblewrap` cannot create the namespace, Wire fails closed instead of silently running on the host.

Set `WIRECLI_ALLOW_PORTABLE_SANDBOX=1` only for an explicit degraded run on systems where OS sandboxing is unavailable.

Commands that need network access, direct shells, inline interpreters, permission changes, or long-running listeners create an approval request. Use:

```bash
wirecli approvals list
wirecli approvals allow-once <request-id>
wirecli approvals allow-repo <request-id>
wirecli approvals deny-always <request-id>
```

Approval decisions are scoped to the current project key and every request/decision/use is appended to `approval_audit.jsonl`.

## Runtime checkpoints and verification

Wire persists runtime checkpoints for model requests, streamed model turns, tool batches, verifier results, and provider empty/error states in the session timeline. If a provider stops after tools or returns an empty follow-up, the next continuation is anchored to the last saved tool checkpoint instead of restarting the task.

After file-editing tools (`apply_patch`, `write_file`, `replace_in_file`, `delete_file`, `copy_file`, `move_file`), Wire runs a native verifier pipeline inside the Box with network disabled. The pipeline always tries `git diff --check` for git workspaces and infers project validators such as `cargo fmt --check`, `cargo test`, package `lint`/`test`/`build` scripts, `go test ./...`, and `python3 -m pytest` when matching project files exist. The verifier report is appended to the tool result, persisted as `verifier.pipeline`, and shown when a session is resumed.

Verifier reports include edited paths, commands run, skipped validators, status, and an undo hint. In git workspaces the hint points to `git diff -- <paths>` for inspection and `git restore -- <paths>` for tracked-file rollback.

## Subagents and recoverable tool failures

Wire exposes a native `subagent` tool with scoped roles: `planner`, `codebase_researcher`, `patcher`, `reviewer`, `test_runner`, and `security_auditor`. Subagents do not expand or reduce the main agent's permissions; they run bounded analysis with no network and no direct file edits, then return a report to the main agent.

Tool execution failures, approval-required commands, denied commands, sandbox blocks, and verifier failures are returned to the model as recoverable tool output. Wire should not stop the agent just because a tool or command failed. If the provider repeats the exact same failing tool call, Wire records a checkpoint and asks the model to choose another allowed route instead of ending the session.

## ACP/WDF parser recovery

Wire's Agent Completion Protocol (ACP) enforces the valid sequence: assistant text or tool call, Wire tool result, then assistant final text, updated plan, or next tool call. A provider that returns an empty assistant turn after tool results, malformed SSE/JSON, or repeated recoverable tool failures is routed through Watch Dog Fixes (WDF).

WDF stops consuming malformed streams, records `acp_wdf_repair` checkpoints, and sends a narrow recovery prompt so the model continues from the latest checkpoint instead of restarting or ending the agent.

## Lifecycle hooks

Wire hooks are first-class lifecycle events stored in `~/.wirecli/hooks.json`. They run inside the active Box through the same sandbox and approval policy as normal commands, and hook failures are persisted as `hook.event` timeline entries instead of stopping the agent.

Core events are `session_start`, `pre_tool_use`, `post_tool_use`, `file_changed`, `pre_compact`, `stop`, `stop_failure`, and `permission_request`. Legacy events `after_shell`, `after_edit`, and `after_commit` remain supported. Hook records can be scoped by command, tool name, lifecycle status, and changed path so validation and audit automation stays narrow.

## Context Budget

Wire CLI is still in beta. Large or premium models are not recommended for day-to-day work yet, especially Opus-class models on OpenRouter. Prefer smaller models until the agent loop, cost guards, and tool-routing behavior are mature enough for expensive long-context runs.

`wirecli status` shows the active model context window, maximum completion budget, estimated latest-session usage, and remaining context. Wire CLI also auto-compacts long sessions through the configured provider before the active model hits its context limit.

## Skills

Local skills live in `~/.wirecli/skills/<skill-name>/SKILL.md`.
Wire installs a default `skill-creator` skill and a technical reference at `~/.wirecli/skills/SKILL_CREATOR.md`.
When a user mentions `@skill-creator`, the agent should read that reference and create the requested skill. If scripts are needed, Python is preferred when available, then Node.js; if neither exists, the agent asks which runtime to use.

## Build

```bash
cargo build
```
