# runwatch

[![CI](https://github.com/m2selfA/runwatch/actions/workflows/ci.yml/badge.svg)](https://github.com/m2selfA/runwatch/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Windows-first **Durable Run Lifecycle Authority** for long scientific computation. `runwatchd` owns canonical Run/Attempt/Observation/Delivery state, remote scheduler lifecycle and durable continuation while the initiating coding agent may be gone.

The v0.1.0 release is Pi-first: `runwatch` is the durable authority, `pi-runs` is the Pi integration plane, and `pi-ssh-tools` is the online remote workspace plane. See [installation](docs/INSTALL.md) and the [v1 release contract](docs/V1_RELEASE_CANDIDATE.md) before deploying it.

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
| `runwatch-mcp` | `runwatch-mcp` | generic stdio MCP surface; Pi v1 uses `pi-runs` local IPC instead |

## Data

`%USERPROFILE%\\.runwatch\\runwatch.db` is the canonical SQLite WAL store. `config.yaml` remains the local configuration file; a legacy `runs.jsonl`, if present, is migration input only.
The legacy importer is one-way/read-only: it is considered only when the canonical runs table is empty. Historical callback fields are ignored during deserialization and are no longer part of the current Run/MCP schema.

## Current release scope

The first production release is intentionally **Pi-first**: `pi-runs` is the Pi Agent Integration Plane and `pi-ssh-tools` is the Pi-online remote workspace plane. The real Pi + runwatch + Slurm/Local Process continuation path is already functional; current work is distribution/install readiness, repeatable release acceptance, endurance testing and compatibility retirement. New coding-agent integrations are deferred until this v1 path is closed.

Remote Slurm/LSF `workspace.cwd` is part of the durability contract: it must be a persistent filesystem visible at the same path from the SSH login node and scheduler compute nodes. runwatch places its attempt script, stdout/stderr, terminal sentinel and receipt under `<workspace>/.runwatch/<run_id>/`; node-local `/tmp` or scratch is therefore unsupported unless the cluster explicitly makes that path shared.

## Build

```text
cargo build -p runwatch-cli --release
cargo build -p runwatch-gui --release
```

The GUI is `#![windows_subsystem = "windows"]`, embeds `assets/icon.ico`,
hides to the tray on close and can install a Startup-folder shortcut. The project is migrating toward a single `runwatchd` owner with local IPC; see `docs/DEVELOPMENT_CHECKPOINT.md` for the exact current boundary.
Windows install/upgrade/uninstall semantics are documented in `docs/INSTALL.md`. Resident removal is stop-then-unregister and fails closed if an independent owner remains; it does not cancel durable scientific Runs or remove `%USERPROFILE%\\.runwatch` state.

```text
runwatch serve --interval 20
runwatch wait RUN_ID --timeout 3600
runwatch-mcp          # stdio MCP: list_runs, submit_run, tick, wait_run, list_hosts
runwatch-gui
```

## Validated Codex reference adapter (frozen for v1)

R9/R10 proved that the continuation model can support Codex as a structurally different second adapter: exact MCP thread binding, persisted rollout evidence and offline exact-thread resume all passed real-provider acceptance. This code remains regression-covered reference evidence, but **Codex is not part of the current v1 release gate and no further Codex productization is planned until runwatch + pi-runs are complete**. A future design would move Codex-specific integration into a separate Agent Integration project rather than further coupling it to runwatch.

For a packaged install where `runwatch`, `runwatch-mcp`, and the GUI live beside one another:

```text
runwatch agent codex status
runwatch agent codex install
runwatch agent codex doctor
runwatch agent codex remove
```

`install` is idempotent and refuses to overwrite an unrelated MCP entry named `runwatch`. `doctor` is read-only: it checks the native Codex launcher, sibling `runwatch-mcp`, the persisted-session root, owned/enabled MCP registration, and daemon compatibility. A fresh Codex install with no sessions yet is allowed; an unavailable or incompatible daemon is reported separately.

### Historical/explicit Codex reference acceptance

The release harness performs a **real Codex provider turn and a real short Slurm job**. Run it only when those external effects are intended:

This is a reference/diagnostic gate, not a blocker for the Pi-first v1 release.

```text
python scripts/acceptance/codex_real_provider.py \
  --confirm-real-provider \
  --host <ssh-alias> \
  --workdir /absolute/remote/workdir
```

## Development

The repository is Rust-first. Run the local validation matrix before opening a change:

```text
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
```

GitHub CI keeps the Windows and Linux compatibility contracts separate. Windows builds/tests the complete workspace and verifies the three-binary `xtask` ZIP. Linux builds/tests the non-GUI runtime (`runwatch` + `runwatch-mcp`) inside the `manylinux2014_x86_64` baseline and then inspects ELF version requirements with `readelf`; CI fails if either binary requires a `GLIBC_*` symbol newer than **2.17**. The Linux CI artifact is currently a binary artifact, not yet the formal three-sibling release package; `runwatch-gui` remains Windows-only.

Build and verify the portable Windows package with the Rust-native release tool:

```text
cargo run -p xtask -- package
cargo run -p xtask -- verify dist/runwatch-v<version>-<platform>.zip
```

See [docs/INSTALL.md](docs/INSTALL.md) for the supported sibling-binary installation layout and [AGENTS.md](AGENTS.md) for project boundaries.

## License

runwatch is released under the [MIT License](LICENSE).
