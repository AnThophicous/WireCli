# Rift CLI

Rift CLI is the public name. Rift Code is the internal runtime.

The execution layer is called **Rift Lattice**: isolated **Boxes** with their own workspace, command trace, and bubblewrap-backed runtime.
Current target platforms: Linux and Windows.

## Commands

- `init`
- `status`
- `doctor`
- `sessions`
- `run <prompt...>`
- `resume [session-id]`
- `config show|get|set`
- `box new|list|info|run|destroy`
- `lattice new|list|info|run|destroy` as alias

## Build

```bash
cargo build
```
