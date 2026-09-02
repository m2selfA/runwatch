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
4. Start the resident runtime if you want unattended continuation across coding-agent exits:

```text
runwatch.exe autostart --install
runwatch.exe daemon-status
```

The Task Scheduler entry runs as the current interactive user so the daemon sees the same SSH config, keys and agent environment. `runwatch autostart --remove` removes the resident task.

## Pi + pi-runs

Install `pi-runs` in Pi separately, then keep `runwatchd` reachable through the normal local IPC endpoint. `pi-runs` is not a second daemon manager and does not copy runwatch binaries into the Pi package. Its default `auto` backend uses runwatch only and fails closed if the daemon/capability contract is unavailable; legacy paths require explicit opt-in.

Inside Pi, `runs_doctor` is the read-only v1 readiness check. A healthy installation reports `ready=true`, `selected_backend=runwatch`, protocol 1, `service=runwatchd`, `storage=sqlite-wal`, and no missing Pi v1 capabilities. It never installs or starts runwatch; if the resident daemon is absent, start/fix the runwatch service rather than switching to legacy.

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
