# Installing runwatch

runwatch is distributed as a **sibling-binary package**. Keep these files in the same directory:

```text
runwatch[.exe]
runwatch-mcp[.exe]
runwatch-gui[.exe]
```

Keep the three binaries together. The v1 Pi path uses the `runwatch` daemon/local IPC directly through `pi-runs`; `runwatch-mcp` remains part of the portable package as the generic MCP surface and for the already-validated reference adapter work. Moving individual binaries out of the package is unsupported.

## Windows quick start

1. Extract the release ZIP to a stable per-user directory. Do not run directly from the ZIP viewer.
2. Optionally add that directory to `PATH`.
3. Keep your cluster aliases in the normal OpenSSH `~/.ssh/config`; runwatch does not maintain a second host database.
   For Slurm/LSF Runs, choose a persistent shared workspace (for example a cluster home/project filesystem) that is mounted at the same path on login and compute nodes. Do not use login-node `/tmp` or node-local scratch unless the cluster explicitly documents it as shared; runwatch's sentinel/log/artifact paths must remain visible after the scheduler job runs.
4. Start the resident runtime if you want unattended continuation across coding-agent exits:

```text
runwatch.exe autostart --install
runwatch.exe daemon-status
```

The Task Scheduler entry runs as the current interactive user so the daemon sees the same SSH config, keys and agent environment. `runwatch autostart --remove` is a **stop-then-unregister** operation: it asks Task Scheduler to end the owned task, verifies the resident supervisor/serve ownership locks, and if the Task action host left its supervisor child alive, terminates only the verified supervisor PID before deleting the registration. It deliberately does not use `taskkill /T`, so durable breakaway Local Process workloads are not treated as service children to cancel.

## Pi + pi-runs

Install `pi-runs` in Pi separately, then keep `runwatchd` reachable through the normal local IPC endpoint. `pi-runs` is not a second daemon manager and does not copy runwatch binaries into the Pi package. Its active `auto|runwatch` backend uses runwatch only and fails closed if the daemon/capability contract is unavailable; the old `legacy` backend is retired and also fails closed.

Inside Pi, `runs_doctor` is the read-only v1 readiness check. A healthy installation reports `ready=true`, `selected_backend=runwatch`, protocol 1, the daemon `version` for diagnostics, `service=runwatchd`, `storage=sqlite-wal`, and no missing Pi v1 capabilities. Compatibility is protocol/capability based rather than exact-version based. It never installs or starts runwatch; if the resident daemon is absent, start/fix the runwatch service.

## Upgrade

1. Close the tray GUI so `runwatch-gui.exe` is not holding the package directory. If GUI logon autostart is enabled, its Startup entry may remain in place for an in-place upgrade to the same directory.
2. From the **currently installed/old** package, run `runwatch.exe autostart --remove`. Do not replace binaries first: the old executable owns the Task Scheduler registration and knows how to stop its resident supervisor safely.
3. Replace the entire sibling package directory as one unit; do not mix versions of `runwatch.exe`, `runwatch-mcp.exe`, and `runwatch-gui.exe`.
4. From the new package, run `runwatch.exe autostart --install`, then `runwatch.exe daemon-status` and Pi `runs_doctor`.

Stopping the resident runtime for upgrade does **not** request Run cancellation. Remote Slurm/LSF jobs continue independently, and Windows Local Process workloads are created with the durable breakaway contract; the new daemon resumes observation after installation. If `autostart --remove` reports that an independent runtime still owns a lock, fix/stop that runtime explicitly rather than deleting the Task Scheduler registration underneath it.

## Uninstall

1. If GUI logon autostart is enabled, turn it off from the GUI, then exit the tray GUI.
2. Run `runwatch.exe autostart --remove` and confirm `runwatch.exe autostart` reports `daemon=disabled`.
3. The extracted sibling package directory can then be moved to the Windows Recycle Bin.

Uninstall does **not** remove `%USERPROFILE%\\.runwatch`, `runwatch.db`, SSH configuration, Pi configuration, or scientific workspaces. Keep that state for reinstall/audit unless you intentionally decide to retire it separately.

## Codex CLI reference adapter (post-v1 backlog)

Register the packaged MCP binary and check readiness:

These commands remain available because R9/R10a/R10b are validated reference code, but Codex is not a blocker for the current Pi-first v1 release and further Codex productization is frozen until runwatch + pi-runs v1 is complete.

```text
runwatch.exe agent codex status
runwatch.exe agent codex install
runwatch.exe agent codex doctor
```

`install` is idempotent and refuses to overwrite an unrelated Codex MCP entry named `runwatch`. `doctor` is read-only and reports the native Codex launcher, sibling MCP binary, persisted-session root, owned/enabled MCP registration and daemon compatibility. A fresh Codex installation with no saved sessions yet is allowed.

To remove only a runwatch-owned Codex registration:

```text
runwatch.exe agent codex remove
```

Unattended Codex continuation never adds dangerous approval/trust bypass flags. Missing/ambiguous persisted thread metadata fails closed instead of creating a replacement conversation.

## GUI

Run `runwatch-gui.exe` from the same extracted directory. GUI startup and resident daemon startup are separate settings: the GUI is a client, while `runwatchd` owns durable Run state, SSH observation and continuation delivery.

## Release integrity

Every package contains:

- `release-manifest.json` with version/platform, the sibling layout contract, file sizes and SHA-256 hashes.
- `SHA256SUMS.txt` covering the packaged payload and manifest.

Formal packaging is Rust-native:

```text
cargo run -p xtask -- package
cargo run -p xtask -- verify dist/runwatch-v<version>-<platform>.zip
```

The packager builds `runwatch`, `runwatch-mcp` and `runwatch-gui`, writes a deterministic ZIP with a single package root, and refuses to overwrite an existing archive. `verify` checks the manifest, every payload size/hash, the exact `SHA256SUMS.txt` coverage, required sibling binaries and unexpected archive files. Existing Python packaging/smoke experiments are prototypes/reference material only and are not the release architecture.
