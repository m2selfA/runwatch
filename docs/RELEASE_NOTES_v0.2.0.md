# runwatch v0.2.0 Release Notes

Release date: 2026-09-04

## Overview

`runwatch` v0.2.0 keeps the v0.1.0 durable-authority architecture and turns the Windows GUI from a validation shell into a practical **Human Run Console**. `runwatchd` remains the only owner of canonical Run/Attempt/Observation/Delivery state, scheduler lifecycle, SSH observation and agent continuation. The GUI talks to it through local IPC; it does not open SQLite directly, create its own scheduler/SSH authority or synthesize Pi/Codex continuation identity.

The historical Pi-first v0.1.0 release, its tag and formal 7800.220-second endurance authority remain immutable. v0.2.0 is primarily a human desktop control/observation release, plus Windows/Linux CI portability hardening and two release-blocker fixes found during final qualification.

## Human Run Console

- Resizable 1080×720 default desktop console with hide-to-tray behavior and no Windows console allocation.
- Virtualized Runs dashboard with Active / Attention / All filtering, search, Priority/Newest/Name/Host ordering and summary counters.
- Execution state and Observation health remain separate: an unreachable scheduler Run stays Running with unhealthy observation instead of becoming Unknown.
- Runs table cells and all default headers stay single-line; long values clip rather than changing row height.
- Lazy Run detail workspace: Overview, bounded Logs, Artifacts, Timeline and privacy-minimized Continuation status.
- Safe actions: Reload, daemon-owned Probe now, Copy Run ID/Handle/Workspace and confirmed Cancel.
- Multiple simultaneous terminal/attention transitions are deduplicated correctly instead of losing all but one.
- Service page exposes daemon/protocol/capability/PID/pause state, resident registration, GUI startup and sibling-package health while keeping service authority out of Settings.
- Hosts page is a read-only OpenSSH projection with equal-size responsive cards for alias, effective endpoint, ProxyJump route and live Run usage. Opening Hosts does not connect to every machine.

## Manual Human Runs

`New Run` creates ordinary unbound `SubmitRunSpec` requests through the existing daemon `submit_run_v2` authority. It supports:

- Windows Local Process;
- Slurm over an existing OpenSSH alias;
- IBM LSF over an existing OpenSSH alias.

A GUI-authored Run always has `continuation=None`. The form does not accept Pi branch/session or Codex thread identity, so a human desktop action cannot manufacture agent continuation authority.

Name is optional; the GUI creates a readable bounded name and durable `manual-...` Run ID when omitted. Remote workspaces are explicitly documented in the form as persistent paths that must normally be shared between login and compute nodes.

## Windows resident/service hardening

- Real Limited-user desktop qualification fixed elevated-install Task Scheduler ACLs: the current user, SYSTEM and Administrators receive the required management rights.
- Resident removal disables the registration before ending/stopping/deleting it, closing the PT1M trigger resurrection race; failure best-effort restores Enable rather than leaving a half-disabled service.
- Service disable does not cancel scientific work. Remote scheduler jobs and durable Local Process work survive the observation gap.
- Main-window close continues to hide the GUI to tray instead of terminating the resident client.

## CI and portability

- Windows `windows-2022` CI installs/verifies NASM, runs full workspace formatting/check/tests, renders critical WindUI software fixtures, builds the optimized three-sibling ZIP, verifies it and uploads it.
- Linux runtime CI builds/tests `runwatch` + `runwatch-mcp` inside `manylinux2014_x86_64` and hard-gates ELF version needs with `readelf`; shipped Linux binaries remain at or below the required glibc 2.17 ceiling.
- GUI fixture coverage includes dashboard, detail, daemon-offline, New Run and Hosts at 1080×720 plus Hosts at 760×720 and 1440×900.

## R16 release-qualification fixes

Final v0.2.0 qualification exposed two concrete release blockers and fixed both before tagging:

1. **Tall New Run scheduler forms:** Slurm/LSF resource fields could extend below the default 1080×720 viewport and clip the Back / Start Run footer. The form body is now a bounded scroll region inside a fixed-height dialog while the footer remains pinned. Windows CI additionally probes the lower-right `new-run` screenshot region for the primary action, so a valid but clipped PNG can no longer pass.
2. **Scheduler Attempt status drift:** remote scheduler observation updated canonical `RunRecord.status` but could leave the durable current `RunAttemptRecord.status` at its submission value. Slurm/LSF transitions now persist Run + current Attempt together with a `scheduler_observed` Event, matching the already-correct Local Process behavior.

## Real packaged desktop qualification

The exact post-fix local v0.2.0 candidate was exercised from the ordinary Limited `CAP00\\inter` RDP Session 2 through the **packaged GUI**, against an isolated packaged daemon/store:

- Local Process Run `manual-write-output-r16-gui-loc-20260904-102422-fb5049` -> `succeeded`; stdout contains `R16_GUI_LOCAL_OK`.
- gm00 Slurm Run `manual-echo-r16-gui-slurm-ok-sl-20260904-102435-4d0b1d` -> Job **31838** -> `succeeded`; bounded stdout contains `R16_GUI_SLURM_OK`.
- gyz-mn02 IBM LSF 10.1 Run `manual-echo-r16-gui-lsf-ok-lsf-20260904-102448-218ced` -> Job **79289** -> `succeeded`; bounded stdout contains `R16_GUI_LSF_OK` from shared `/scem/work/gaoyz/runwatch-r16-release`.

All three retained synchronized durable Attempt state and the isolated store contained **zero ContinuationBindings**.

## v0.1.0 -> v0.2.0 upgrade qualification

The frozen historical v0.1.0 package (SHA-256 `3b67fae3055bed5765ae587fd9b27d228ea84fb47f1a0c4c377a0317591e426f`) was installed into an isolated sibling directory and started against an isolated canonical SQLite authority. It submitted gm00 Slurm Job **31837** (`sleep 45; echo R16_UPGRADE_FINAL_OK`).

While Job 31837 was confirmed RUNNING, the old resident runtime was stopped and the entire sibling install directory was replaced by the final local v0.2.0 package. The same data directory/IPC authority restarted as version 0.2.0; the same Run immediately remained Running on the same JobID, then converged to `succeeded`. Final bounded logs contained `R16_UPGRADE_FINAL_OK`; SQLite retained exactly one Run + one Attempt, schema version 2, with **Run=succeeded / Attempt=succeeded / JobID=31837**.

This package-replacement gate was deliberately isolated from the user's production `runwatchd` registration. The real fixed-name Task Scheduler stop/re-enable, PT1M non-resurrection and elevated-install ACL behavior were already qualified from the ordinary Limited desktop during R13.

## Release artifact

Pre-CI post-fix local candidate:

- Archive: `runwatch-v0.2.0-windows-x86_64.zip`
- Size: **10,359,397 bytes**
- SHA-256: `0e0d52d01c4a000aa3918a7231c205f11f8c90de1fb24397745abb3ef428f9bd`
- `cargo run -p xtask -- verify ...`: `ok=true`, platform `windows-x86_64`, version `0.2.0`, files=5.

Fresh-runner release identity from implementation commit `d070eba2389e79ffcce4e57b2ffbfc7f03e9dec5` / GitHub Actions **#33863768677**:

- Windows GitHub artifact ID **9933490617**, **10,292,303 bytes**, digest `sha256:b2782e2753816fdcde93ede2be8d0aa18926d68a89bef3942267ac119c6e30f0`.
- Authoritative public inner ZIP: `runwatch-v0.2.0-windows-x86_64.zip`, **10,303,408 bytes**, SHA-256 `e662cf8c5793aa562ab3a55cd8ca0a73241482ea6b85dda2339aedb0d3364eb1`; downloaded copy independently passes `xtask verify` (`ok=true`, files=5, version=0.2.0, platform=windows-x86_64).
- Linux artifact ID **9933223985**, **6,129,423 bytes**, digest `sha256:4efce7a9e14280d1583cc18183aa52ca2fa7c87ee22396de78efbabcd4675714`; both shipped ELFs require at most `GLIBC_2.16 <= GLIBC_2.17`.

Fresh Windows CI also passed **127 tests / 0 failed / 8 ignored**, the strengthened seven-case GUI fixture matrix (including the pinned Start Run footer gate), optimized package build and package verification. The only workflow annotation is GitHub's Node-20-to-Node-24 action-runtime platform notice.

## Compatibility and installation

Local IPC remains `protocol_version=1`, `service=runwatchd`, `storage=sqlite-wal`; compatibility remains protocol/capability based rather than exact semver matching. Existing Pi-first v0.1 continuation semantics remain supported.

See `docs/INSTALL.md` for install/upgrade/uninstall. In-place upgrades must stop the resident runtime using the old package, replace the sibling package directory as one unit, then install/start the new runtime. `%USERPROFILE%\\.runwatch` remains canonical state and is not removed by upgrade/uninstall.

## Known non-blocking debt

- WindUI 0.14 does not expose a public runtime tray-handle/tooltip mutation bridge, and controller-originated native Windows notifications are therefore still deferred rather than implemented through brittle private Win32 handle discovery.
- Persistent notification/display preferences remain future UX work and must never become Run/Delivery authority.
- Retry/resubmit and multi-Attempt management UI are deliberately deferred to the next development phase; v0.2.0 does not invent those semantics during release closure.
- Rust still reports the existing `russh 0.54.5` future-incompatibility warning; current builds/tests are green.
- More third-party MCP client interoperability remains release hardening breadth, not protocol-modernization debt.
- Linux CI currently publishes the two non-GUI runtime binaries; the formal three-sibling end-user package remains Windows-only.
- Existing Codex-specific reference code remains frozen evidence. New agent integrations should be extracted into independent Agent Integration projects rather than expanding runwatch's durable authority.
