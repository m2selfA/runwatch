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

The first qualified Windows artifact was 9,683,276 bytes with SHA-256 `33d4b79377caad717ab32aadc1d3599c6b50b860e9ba8c7461309295d8ac7198`. The current HEAD `4888d39` candidate was rebuilt after the SSH-config/fault-hardening work and independently verified at **9,686,896 bytes**, SHA-256 `7bcfe151b56400de2695ff6e8f56f13743691e77c48cda282098b95295edd8f1`.

## Required release evidence

Already passed:

- Rust fmt/check/unit matrix: 105 passed / 0 failed / 8 ignored after compatibility, resident-stop and explicit-SSH-config coverage;
- packaged supervisor readiness and serve-child restart;
- real Pi/provider Windows Local Process release gate;
- real Pi/provider gm00 Slurm release gate on `/share/home/shark/tmp`;
- exactly-once Delivery/AgentInvocation/session settlement checks;
- mixed Local + Slurm resident restart qualification (173.802 s);
- Windows resident stop/unregister acceptance: Task Scheduler `/End` was observed to leave its supervisor child alive, then termination of only the verified supervisor PID released both supervisor/serve ownership through the Job Object; package replacement is therefore gated on owner-lock release rather than Task Scheduler status alone.
- focused same-session branch divergence/rebind with exactly one live completion + settlement after explicit `runs_rebind`;
- focused completion-before-settlement daemon crash recovery with no duplicate completion;
- focused real gm00 SSH transport interruption/recovery: Job 31753 remained running while Observation changed `fresh -> unreachable -> fresh`, then completed with exactly-once continuation settlement;
- current-package repeat qualification: 456.473 s, 2 rounds / 4 real cases, concurrent Local + Slurm, two serve restarts, two same-session rebind recoveries, Slurm Jobs 31755/31756 and exactly-once final remote continuation. It is intentionally not counted as the multi-hour gate.
- focused Slurm-only packaged rebind on Job 31757 with explicit pi-runs + pi-ssh-tools extension loading: zero wrong-branch completion/settlement, exactly one rebind, Delivery attempt 2 final success and exact remote result inspection.
- resumable-endurance harness qualification: two real Local Process segments reused the same durable SQLite/IPC authority with rounds 1 -> 2 and 193.195 s cumulative clean active time; frozen code/product/fault hashes reject changed-contract resume before any next segment is created, and dirty segment history cannot be washed out by later successes.
- first formal 7200-second session `soak-20260902153301-b21f6ed7` is preserved as failed/non-resumable evidence: round 1 passed Local + Slurm Job 31758 with resident restart; round 2 experienced real Pi provider 524/524/503 latency while short Slurm Job 31759 finished before the scheduled SSH cut. The harness failed closed, marks `failed:1`, and credits zero endurance time.
- formal fault endurance now freezes an explicit seed/workload timing invariant: bounded seed timeout is separate from round timeout, run delay must exceed seed timeout by >=60 seconds for a >=7200 s target, and Slurm walltime equals delay + 120 seconds. Current next-session profile is delay 600 / seed timeout 480 / round timeout 1200.
- clean formal session #2 `soak-20260902155301-4ddcb294` segment 1 passed: 1004.342 s, Local + Slurm Job 31760, both active across serve 50636 -> 84340, both exactly-once terminal continuations, and zero dirty segments. The same evidence session must accumulate the remaining duration and repeated fault coverage.

Still blocking a v1 tag:

- one **new, clean** multi-hour endurance evidence session from the current packaged layout whose read-only evaluator returns `v1_endurance.qualified=true`. The dirty `b21f6ed7` session is permanently excluded. All individual fault dimensions are qualified; duration/repetition under one frozen resident authority is the remaining gate.

No human `continue` message is permitted in the formal continuation gates.
