# runwatch

Windows-first watcher for durable **runs** on SSH hosts and HPC login nodes.
Keeps a multiplexed russh session per `~/.ssh/config` Host alias, polls job
completion, and can wake an agent.

- 痛点：[docs/pain-points.md](docs/pain-points.md)
- 设计：[docs/design.md](docs/design.md)

## Crates

| crate | binary | role |
|---|---|---|
| `runwatch-core` | | ledger, probe maps, autostart |
| `runwatch-ssh` | | russh pool + ssh_config |
| `runwatch-engine` | | shared tick / wait / serve loop |
| `runwatch-cli` | `runwatch` | console CLI for agents |
| `runwatch-gui` | `runwatch-gui` | windui tray + main window (no console) |
| `runwatch-mcp` | `runwatch-mcp` | stdio MCP for Pi / Claude / Codex |

## Data

`%USERPROFILE%\\.runwatch\\runwatch.db` is the canonical SQLite WAL store. `config.yaml` remains the local configuration file; a legacy `runs.jsonl`, if present, is migration input only.

## Build

```text
cargo build -p runwatch-cli --release
cargo build -p runwatch-gui --release
```

The GUI is `#![windows_subsystem = "windows"]`, embeds `assets/icon.ico`,
hides to the tray on close and can install a Startup-folder shortcut. The project is migrating toward a single `runwatchd` owner with local IPC; see `docs/DEVELOPMENT_CHECKPOINT.md` for the exact current boundary.

```text
runwatch serve --interval 20
runwatch wait RUN_ID --timeout 3600
runwatch-mcp          # stdio MCP: list_runs, submit_run, tick, wait_run, list_hosts
runwatch-gui
```
