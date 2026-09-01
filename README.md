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

## Codex CLI adapter

runwatch can bind a scientific Run to the exact persisted Codex CLI thread that submitted it. Submission uses the Codex MCP tool metadata for thread identity; terminal continuation is owned by `runwatchd` and resumes that same thread offline when needed.

For a packaged install where `runwatch`, `runwatch-mcp`, and the GUI live beside one another:

```text
runwatch agent codex status
runwatch agent codex install
runwatch agent codex doctor
runwatch agent codex remove
```

`install` is idempotent and refuses to overwrite an unrelated MCP entry named `runwatch`. `doctor` is read-only: it checks the native Codex launcher, sibling `runwatch-mcp`, the persisted-session root, owned/enabled MCP registration, and daemon compatibility. A fresh Codex install with no sessions yet is allowed; an unavailable or incompatible daemon is reported separately.

### Real-provider release acceptance

The release harness performs a **real Codex provider turn and a real short Slurm job**. Run it only when those external effects are intended:

```text
python scripts/acceptance/codex_real_provider.py \
  --confirm-real-provider \
  --host <ssh-alias> \
  --workdir /absolute/remote/workdir
```

The harness fixes the remote workload to a two-second acceptance command, stores runwatch state behind a nonce-isolated SQLite/IPC endpoint, and injects the temporary MCP server only through process-local Codex `-c` overrides. It does **not** modify the permanent Codex MCP registration or `config.toml`. Success requires both canonical SQLite evidence (`Run`, `Delivery`, `AgentInvocation`) and exact-thread rollout evidence (one deterministic continuation marker with matching `task_complete`); process exit alone never counts as continuation success.
