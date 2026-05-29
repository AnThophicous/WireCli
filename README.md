# Rift CLI

`riftcli` is the user-facing binary. The internal runtime is Rift Code.

The Rift state lives inside the current project directory under `.riftcode/`.

## Core layers

- **Box**: the writable project workspace the agent acts on.
- **Lattice**: the execution perimeter around the Box.
- **Anchor**: durable memory stored in SQLite.
- **Tide**: the live session history and event stream.
- **Loom**: the context builder that assembles the prompt for each model turn.

## Storage

- `.riftcode/config/config`
- `.riftcode/data/history.sqlite3`
- `.riftcode/data/anchor.sqlite3`
- `.riftcode/boxes/`

## Commands

- `riftcli`
- `tui`
- `run <prompt...>`
- `models`
- `providers`
- `box [new|list|run|tools]`

`riftcli` opens the full-screen TUI by default.

## Memory tools

- `remember` stores durable Anchor memory.
- `recall` looks up relevant Anchor memory.

## Build

```bash
cargo build
```
