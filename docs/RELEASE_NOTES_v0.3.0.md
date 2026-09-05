# runwatch v0.3.0 Release Notes

Release date: 2026-09-05

Published GitHub Release: `https://github.com/m2selfA/runwatch/releases/tag/v0.3.0`.

## Overview

`runwatch` v0.3.0 is the durable multi-Attempt and Human Retry release. It keeps the single-writer architecture from v0.1/v0.2: `runwatchd` owns canonical Run/Attempt/Observation/Delivery state, scheduler lifecycle, retry allocation/submission and continuation delivery; GUI/MCP/CLI remain local-IPC clients.

The major product change is that one logical Run may now own multiple durable Attempts without overwriting earlier evidence. Human users can review and Retry eligible Failed/Cancelled unbound Runs while the daemon guarantees idempotent N+1 allocation, crash-safe submission recovery and fail-closed agent identity boundaries.

v0.3.0 also publishes the completed runwatch-local portion of the Desktop Attention work: WindUI 0.15, dynamic tray status, notification policy/coalescing and disposable GUI-local settings. Native background OS notifications are **not enabled in this release** because WindUI 0.15.0 does not yet expose the required public runtime notification method; the UI says so explicitly and keeps those controls disabled.

## Durable multi-Attempt lifecycle

- Run remains the stable logical/scientific object; Retry creates Attempt N+1 under the same Run ID.
- Attempt history is durable and earlier stdout/stderr/artifacts/Observation remain readable.
- New execution evidence uses attempt-scoped `.runwatch/<run_id>/attempt-N/` paths; existing v0.2 persisted paths remain authoritative without migration.
- GUI exposes Attempt history/selection, Attempt-scoped Observation/Logs/Artifacts and Run-global Timeline.
- SQLite schema advances from 2 to **3** and opens schema-2 stores in place by adding durable `retry_intents`.

## Human Retry safety

- Retry is daemon-owned through `retry_run_v1` with `run_id`, expected Attempt number and durable request ID.
- N+1 allocation occurs in an immediate SQLite transaction; replay of one request ID cannot create extra Attempts.
- Only current Failed/Cancelled **unbound** Runs are human-retry eligible.
- Any ContinuationBinding or residual agent/session/project identity fails closed; workflow retry of agent-bound Runs remains owned by the Agent Integration.
- Command/workspace/runner/host stay fixed for normal Retry. Slurm/LSF resource envelopes may be reviewed/adjusted; Local Process has no scheduler envelope override.
- Submission acceptance is compare-and-set: same-handle followers are idempotent, different handles conflict, and stale queued acceptance cannot regress a newer Running/terminal state.

## Crash and ambiguous-return recovery

R17 qualification exercised the failure boundaries that matter for duplicate scientific execution:

- durable Retry intent allocated before scheduler submission, followed by daemon crash/restart;
- scheduler accepted submission but local response was intentionally lost;
- Local Process resident restart after durable Retry allocation;
- concurrent foreground Retry and daemon recovery adopting the same receipt.

The formal final authority `r17-final-596323c-20260905` recorded **7223.155 clean seconds**, **12 rounds**, **4 clean segments**, **0 dirty segments** and **0 bad rounds**. Every round ran failed Attempt 1 -> successful Attempt 2 for both Local Process and real Slurm, killed/replaced the resident while retry work was active, replayed the same retry request, preserved Attempt-1 logs and proved exactly one Attempt-2 scientific execution. Real IBM LSF Retry also passed.

## Desktop attention and tray UX

- WindUI upgraded from 0.14 to **0.15.0** without GUI compatibility breakage.
- Existing tray tooltip now follows canonical DashboardSnapshot state: idle, active count, attention count, paused or daemon unavailable.
- Tray projection has no duplicate lifecycle counter; it derives from the same dashboard read model.
- Transition policy distinguishes background Run terminal/attention changes from ordinary action-result toasts.
- Eligible transitions are coalesced to at most one native intent per refresh and are privacy-minimized.
- Versioned `gui-settings.json` stores only desktop preferences. Missing/corrupt/future-version files fail soft to defaults and never affect Run/Delivery truth.
- Limited RDP Session-2 packaged acceptance covered corrupt, valid and missing settings across GUI restart plus hide-on-close resident behavior.

### Native notification limitation

Controller-originated native background notifications remain disabled in v0.3.0. WindUI 0.15.0 has public `TrayHandle::set_tooltip()` but not runtime `TrayHandle::notify()`. runwatch deliberately does **not** use private Win32 HWND discovery, create a second tray icon, or pin a private framework fork. The minimal upstream bridge is tracked as `huanfeng/wind-ui-rust#13`; the exact contribution head passed WindUI library tests on Windows (**746/746**) and macOS x86_64 (**697/697**). A runwatch integration rehearsal against that exact head also passed full tests/package verification, so this is an external framework gate rather than hidden runwatch design debt.

## Compatibility and upgrade

Local IPC remains `protocol_version=1`, `service=runwatchd`, `storage=sqlite-wal`; compatibility remains capability-based rather than exact-semver matching. Existing Pi-first continuation behavior remains supported.

The canonical SQLite store advances to schema 3. A regression creates a schema-2 store, opens it with current code, confirms metadata becomes 3 and confirms `retry_intents` exists. Existing v0.2 Attempt paths are retained exactly.

See `docs/INSTALL.md` for install/upgrade/uninstall. Upgrade the three Windows sibling binaries as one package: stop/unregister the resident runtime with the old `runwatch.exe`, replace the package directory, then install/start the new runtime. `%USERPROFILE%\\.runwatch` remains canonical state and is not removed by upgrade/uninstall.

## CI and packaging

The release keeps the existing hard gates:

- Windows `windows-2022`: rustfmt, workspace check/tests, R17 endurance-driver self-check, critical GUI software fixtures, optimized three-sibling package, `xtask verify`, artifact upload.
- Linux: manylinux2014/glibc-2.17 baseline, tests/release build and ELF symbol-version ceiling.

Pre-CI local candidate: `runwatch-v0.3.0-windows-x86_64.zip`, **10,479,597 bytes**, SHA-256 `fc8cff6e80ea45b590866720287a0845a009ea87c26025ed86d00db46398c64f`; independent `xtask verify` reports `ok=true`, files=5, version `0.3.0`, platform `windows-x86_64`.

Fresh-runner qualification: implementation/release-document commit `bf86eaa191971eeb513b9b6acbe7daac2d66da4d` passed GitHub Actions **#33949072262**. Windows passed formatting, workspace check, **140 passed / 0 failed / 10 ignored**, R17 endurance-driver validation, critical GUI fixtures, optimized packaging, package verification and upload; Linux passed the glibc-2.17 baseline build/test/release + upload gate. Windows artifact **9964382254** is 10,419,230 bytes with outer digest `sha256:5f2a6ea768741c92ed21cd3591b58e02f029b122ce1f668ff1d3fb6801fa9108`. Its independently downloaded authoritative inner `runwatch-v0.3.0-windows-x86_64.zip` is **10,429,395 bytes**, SHA-256 `cfbc3f3b16876e2da3a7096e3f8d7c5b7f55bb248ab073f2abbab0a268e5688f`, and independently re-verifies `ok=true`, files=5, version `0.3.0`, platform `windows-x86_64`. Linux artifact **9964275232** is 6,206,678 bytes with digest `sha256:d40942b82152d43049e8d13a0a784b9705e79dee72d1d835bf87c4c5eebdec7c`. Annotated tag object `5dcf420dd11144c613bdfc1c95333d18cdd89622` peels to qualification commit `aa64ad2bbbf4dc68891b04d6114ed7f93cc5d624`. GitHub Release **383155763** publishes asset **545458756**, `runwatch-v0.3.0-windows-x86_64.zip`, with the same **10,429,395-byte** size and `sha256:cfbc3f3b16876e2da3a7096e3f8d7c5b7f55bb248ab073f2abbab0a268e5688f` digest.

## Known non-blocking debt

- WindUI runtime native-notification public API and final packaged native-popup acceptance remain R18 follow-up work; native OS notification controls stay disabled in v0.3.0.
- Rust still reports the existing `russh 0.54.5` future-incompatibility warning; current builds/tests are otherwise green.
- Linux CI publishes runtime binaries rather than the formal Windows three-sibling end-user package.
- Existing Codex-specific code remains frozen reference evidence; new agent integrations should be extracted into independent Agent Integration projects rather than expanding runwatch's durable authority.
