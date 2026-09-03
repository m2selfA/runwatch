# runwatch v0.1.0 Release Notes

Release date: 2026-09-04
Qualification completed: 2026-09-03

## Overview

`runwatch` v0.1.0 is the first Pi-first production release of the durable Run lifecycle authority. `runwatchd` owns canonical Run state, scheduler/local-process observation, continuation Delivery state and retry ownership in SQLite WAL storage. The supported Pi product is deliberately split across three components:

- `runwatch`: durable Run authority and Windows resident/runtime package;
- `pi-runs`: Pi integration and continuation adapter;
- `pi-ssh-tools`: Pi-online SSH/workspace inspection plane.

Codex and other AgentAdapters are explicitly post-v1 work and are not required by this release.

## Included product surface

- Windows sibling package: `runwatch.exe`, `runwatch-mcp.exe`, `runwatch-gui.exe`.
- Durable Windows Local Process execution with PID/creation-time identity and breakaway semantics.
- Remote Slurm and LSF execution over the user's existing OpenSSH trust/configuration.
- Shared-workspace contract for remote schedulers: the same persistent path must be visible on login and compute nodes.
- Canonical SQLite-WAL Run store, daemon IPC protocol v1 and capability-based compatibility.
- Resident Windows supervisor with bounded serve restart and verified Task Scheduler ownership/cleanup.
- MCP server on `rmcp 3.1.4` with typed output schemas.
- Exact-session Pi offline continuation, branch divergence protection and explicit `runs_rebind` recovery through the separately installed `pi-runs` package.

## Durability guarantees qualified for v0.1.0

- Terminal Delivery ownership is durable and retry-safe.
- Successful continuation requires exactly-once completion/settlement evidence in the saved Pi session.
- Expired live Delivery claims are transactionally requeued before offline reservation, closing the fast-terminal live-to-offline handoff race found during RC replay.
- Same-session branch divergence fails closed as `needs_rebind`; the wrong branch receives no completion before explicit rebind.
- Completion-before-settlement daemon failure recovers without duplicating the completion message.
- SSH transport loss preserves the scheduler Job identity and recovers observation after connectivity returns.
- Service stop/upgrade does not imply scientific Run cancellation; remote scheduler work and breakaway Local Process work remain outside the supervisor process tree.

## Release artifact

Final fixed Windows package source identity: `f6b75b8`.

- Archive: `runwatch-v0.1.0-windows-x86_64.zip`
- Size: 9,686,763 bytes
- Archive SHA-256: `3b67fae3055bed5765ae587fd9b27d228ea84fb47f1a0c4c377a0317591e426f`
- Extracted `runwatch.exe` SHA-256: `3d1be0ac320947adbd29e5335a7894930e9ba8eb05366ae307ba39ccc0ac16a1`
- `cargo run -p xtask -- verify ...`: `ok=true`, platform `windows-x86_64`, version `0.1.0`, files=5.

The release tag may sit on documentation-only commits after `f6b75b8`; `git diff --name-only f6b75b8..HEAD` was verified to contain only release documentation, so the tagged source tree has no runtime-code drift from the qualified package.

## Final release validation

- `cargo fmt -- --check`: passed.
- `cargo check --all-targets`: passed.
- `cargo test --all-targets`: **106 passed / 0 failed / 8 ignored**.
- Final pi-runs regression: **57 passed / 0 failed / 1 skipped**.
- Explicit real-Pi live bridge: **1/1 passed**.
- Final fixed-package Local Process gate: `local-process-20260903143640-87bf00a7`, one doctor, one submit, Delivery attempt=1, one completed AgentInvocation, completion=1, settlement=1.
- Final fixed-package gm00 Slurm gate: `slurm-20260903143800-2e98a872`, Job **31830**, shared workspace `/share/home/shark/tmp`, Delivery attempt=1, one completed AgentInvocation, completion=1, settlement=1 and exact remote result inspection.
- Formal current-binary endurance authority: `pi-runs/acceptance-output/soak-20260903114134-5b2db744` — **7800.220 s / 11 rounds / 22 real cases / 3 clean segments / 0 failed segments**; Local=11, Slurm=11, serve restarts=11, SSH recoveries=5, branch-rebind recoveries=5, settlement-crash recoveries=3; machine `v1_endurance.qualified=true` with `reasons=[]`.

## Installation and upgrade

See `docs/INSTALL.md`. The runwatch portable package and `pi-runs` are installed separately. Upgrade/uninstall must stop the owned resident runtime before replacing the sibling package and must preserve `%USERPROFILE%\.runwatch` unless the user explicitly intends to remove state.

## Known non-blocking debt

- Rust reports the existing `russh 0.54.5` future-incompatibility warning; current builds/tests are green.
- More third-party MCP interoperability breadth remains future hardening.
- Codex and other non-Pi integrations remain post-v1 projects and must not be folded back into this release branch as a second durable authority.
