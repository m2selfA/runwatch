# runwatch v1 Release Candidate Contract

Status: **contract frozen; not yet taggable**. The Pi-first v1 candidate remains blocked on the R11d endurance matrix and final resident upgrade/uninstall lifecycle closeout.

## Product scope

V1 is Windows-first and Pi-first. The supported local product shape is the sibling Windows package (`runwatch.exe`, `runwatch-mcp.exe`, `runwatch-gui.exe`) plus a separately installed `pi-runs` package. Remote scientific execution is Slurm/LSF over the user's existing OpenSSH configuration; local long execution uses runwatch `Process`. No additional AgentAdapter is a v1 release blocker.

For remote schedulers, `workspace.cwd` must be persistent storage visible at the same path on login and compute nodes. Node-local `/tmp` or scratch is unsupported unless the cluster explicitly guarantees sharing.

## IPC compatibility freeze

Pi v1 requires local IPC `protocol_version=1`, `service=runwatchd`, `storage=sqlite-wal`, and these capabilities:

```text
hello
list_runs
get_run
submit_run_v2
wait_run
logs
artifacts
cancel_run
register_agent_session
release_agent_session
claim_deliveries
delivery_status
ack_delivery
rebind_continuation
verify_offline_invocation
offline_pi_continuation
```

`hello.version` reports the runwatch build version for diagnosis. It is informational: compatibility is determined by protocol identity plus required capabilities, not exact semver equality.
A real isolated cross-project smoke with the rebuilt debug executable returned `version=0.1.0` and pi-runs `ready=true` with no missing capabilities. External compatibility smokes must explicitly build the ordinary executable they launch; `cargo test` rebuilding test harnesses is not sufficient evidence that `target/debug/runwatch.exe` is current.

## Current Run data contract

`runwatch.db` is canonical. A historical `runs.jsonl` is read only when the canonical runs table is empty and is never a second writer. Old JSON keys `on_complete`, `on_success`, `on_failure`, and `acked_at` are tolerated as unknown migration input but are absent from the current `RunRecord`, JSON schema and MCP output schema. Hidden CLI callback arguments only return a retirement error.

## Package contract

The release ZIP must:

- contain one package root and sibling `runwatch`, `runwatch-mcp`, and `runwatch-gui` binaries;
- contain `release-manifest.json` and `SHA256SUMS.txt` with exact payload coverage;
- pass `cargo run -p xtask -- verify <zip>`;
- be byte-deterministic when generated twice from the same release binaries;
- run from the extracted package without relying on `target/` or the source tree.

The first qualified Windows artifact was 9,683,276 bytes with SHA-256 `33d4b79377caad717ab32aadc1d3599c6b50b860e9ba8c7461309295d8ac7198`.

## Required release evidence

Already passed:

- Rust fmt/check/unit matrix: 103 passed / 0 failed / 8 ignored after compatibility-field and resident-stop cleanup;
- packaged supervisor readiness and serve-child restart;
- real Pi/provider Windows Local Process release gate;
- real Pi/provider gm00 Slurm release gate on `/share/home/shark/tmp`;
- exactly-once Delivery/AgentInvocation/session settlement checks;
- mixed Local + Slurm resident restart qualification (173.802 s);
- Windows resident stop/unregister acceptance: Task Scheduler `/End` was observed to leave its supervisor child alive, then termination of only the verified supervisor PID released both supervisor/serve ownership through the Job Object; package replacement is therefore gated on owner-lock release rather than Task Scheduler status alone.

Still blocking a v1 tag:

- true endurance run covering prolonged concurrent workloads, daemon restart, transient SSH loss, terminal scheduler observation and offline Pi relaunch;
- prolonged branch-divergence/rebind and completion/settlement crash-window coverage;

No human `continue` message is permitted in the formal continuation gates.
