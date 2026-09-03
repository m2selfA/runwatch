# runwatch Development Checkpoint

Last updated: 2026-09-03

This file is the authoritative implementation checkpoint for the current redesign. **Every completed development phase must update this document before the next phase begins.**

## Product target

runwatch is the **Durable Run Lifecycle Authority** for long-running scientific computation started by coding agents.

Three-project boundary:

- `runwatch`: durable Run/Attempt/Event/Observation/Delivery state, scheduler lifecycle, narrow persistent SSH, retry and continuation dispatch.
- `pi-runs`: Pi-native tools, Pi session/branch binding, Pi continuation UX. It must not become a second durable scheduler control plane.
- `pi-ssh-tools`: Pi-online remote workspace read/write/edit/shell. It is not a long-lived watcher and is not imported by runwatch.

Shared interop concept:

```text
RemoteWorkspaceRef { host_alias, cwd }
```

Host aliases remain authoritative in the user's `~/.ssh/config`.

## Frozen architecture decisions

1. **Single writer:** final canonical Run state is mutated only by `runwatchd`; CLI/GUI/MCP/agent adapters become local IPC clients.
2. **SQLite WAL:** `runwatch.db` becomes the canonical durable store. JSONL is migration/export only.
3. **Run != Attempt:** logical Run identity survives retries/resubmissions; scheduler JobIDs belong to Attempts.
4. **Execution != observation:** transient SSH/probe failures do not overwrite the last trusted execution state with `unknown/lost`.
5. **Durable delivery outbox:** terminal execution and agent continuation are separate durable phases; callback failure is retryable.
6. **Transport × Runner:** `Local × Process`, `SSH(alias) × Slurm`, `SSH(alias) × LSF`.
7. **Narrow SSH authority:** runwatch uses SSH only for lifecycle operations; arbitrary remote workspace access stays in `pi-ssh-tools`.
8. **Pi-first v1 freeze:** Pi live/offline continuation is the only Agent Integration path that may block the first production release. The validated Codex work remains a reference implementation only; no further non-Pi AgentAdapter productization starts until `runwatch` + `pi-runs` v1 is closed.
9. **Branch-safe continuation:** Pi binding eventually includes `session_file + session_id + origin_leaf_id`; divergence requires explicit rebind.
10. **No hidden trust bypass:** unattended agent resume must not silently approve an untrusted project.

## Milestones

| Phase | Scope | Status |
|---|---|---|
| R0 | Architecture freeze across runwatch / pi-runs / pi-ssh-tools boundary | **completed 2026-08-31** |
| R1 | Durable core: real single-instance lock, SQLite canonical store, local IPC foundation, retryable delivery boundary | **completed 2026-08-31** |
| R2 | Remote Execution v2: Transport × Runner, scheduler adapters, SSH hardening | **completed 2026-09-01 — Slurm/LSF/Local Process, Observation/adaptive batching, OpenSSH trust/ProxyJump/agent policy accepted** |
| R3 | pi-runs backend migration to runwatch local client | **completed for production remote HPC 2026-08-31 — auto is runwatch-only; legacy requires explicit opt-in** |
| R4 | Pi live continuation | **real Pi live terminal-delivery acceptance passed 2026-08-31** |
| R5 | Pi offline continuation via exact-session headless worker | **real exact-session + combined gm00 Slurm/provider-success acceptance passed 2026-08-31** |
| R6 | Branch lineage / rebind safety | **real same-session `/tree` block + explicit rebind recovery passed 2026-08-31** |
| R7 | Fault-injection and unattended release matrix | **core crash/restart matrix completed 2026-08-31; multi-hour soak remains release hardening** |
| R8 | GUI/service/MCP client hardening | **completed 2026-08-31 — GUI/service split, MCP 2026-07-28, supervisor-based Windows resident lifecycle and real fault acceptance; multi-hour soak remains release hardening** |
| R9 | Second-adapter architecture experiment with Codex CLI | **completed 2026-09-01 — exact-thread MCP binding, durable submit, offline exact-thread resume, rollout idempotency and real provider acceptance passed; retained as reference evidence** |
| R10 | Codex onboarding experiment | **frozen after R10b on 2026-09-02 — status/install/remove/doctor remain validated reference code; R10c and further Codex productization are deferred until after runwatch + pi-runs v1** |
| R11 | Pi-first v1 production closure | **in progress 2026-09-02 — release/distribution, repeatable Pi acceptance, soak/fault endurance, compatibility retirement and release-candidate closure** |

## R0 completion record

Completed:

- Reframed runwatch from a generic watcher into the durable Run lifecycle authority.
- Frozen the three authority planes: Remote Workspace (`pi-ssh-tools`), Agent Integration (`pi-runs`), Durable Run (`runwatch`).
- Defined target `Run / Attempt / Event / Observation / Artifact / ContinuationBinding / Delivery` model.
- Defined `RemoteWorkspaceRef { host_alias, cwd }` as semantic interop only; no cross-package SSH dependency.
- Chosen `Transport × Runner` instead of conflating remote location with scheduler.
- Defined Pi live/offline continuation and branch-safe binding as the first end-to-end product gate.

Validation: documentation/architecture phase; code validation belongs to R1.

## R1 current work

### R1a — compatibility durability hardening — completed 2026-08-31

Completed before the SQLite cutover:

- [x] Replaced heartbeat-only `ServeLock` with an OS-released exclusive file lock (`fs2`). A second daemon owner fails immediately; heartbeat remains liveness metadata only.
- [x] Replaced truncate/rewrite `runs.jsonl` behavior with a locked append-only compatibility journal. Readers collapse records by `run_id` to the latest version.
- [x] Shared/exclusive store locks now prevent concurrent load/modify/write lost updates in the compatibility layer.
- [x] Every append is `sync_all()`'d; an interrupted partial final line is ignored while earlier malformed lines remain hard errors.
- [x] Terminal execution state is persisted before wake side effects.
- [x] Terminal rows with an unacknowledged delivery are reconsidered on later ticks, so first-attempt wake failure is retryable.
- [x] Unsupported `OnComplete::Event` now fails closed and remains pending instead of being falsely acknowledged.
- [x] Added regression tests for daemon lock exclusivity, append-journal latest-state semantics, partial-final-line crash recovery, and callback retry selection.

Validation:

- `cargo fmt` — passed.
- `cargo check --all-targets` — passed; only existing `russh v0.54.5` future-incompatibility warning remains.
- `cargo test --all-targets` — **8 passed, 0 failed** (previous project baseline had only 2 tests).

### R1b — SQLite canonical store + read IPC foundation — completed 2026-08-31

Completed:

- [x] `RunStore` now uses `%USERPROFILE%/.runwatch/runwatch.db` as the canonical store.
- [x] Enabled SQLite WAL mode, `synchronous=FULL`, foreign keys and bounded busy timeout.
- [x] Existing latest `runs.jsonl` state is imported automatically when the SQLite store is empty; R1a-style partial final lines are tolerated during migration.
- [x] JSONL is no longer the canonical writer path.
- [x] Created schema foundations for `runs`, `run_attempts`, `run_events`, `observations`, `deliveries`, `continuation_bindings`, `artifacts`, and `agent_session_leases`.
- [x] Added protocol-versioned local IPC server to `serve_loop`.
  - Windows endpoint: `\\.\pipe\runwatch-v1`.
  - Initial v1 capabilities are deliberately read-only: `hello`, `list_runs`, `get_run`.
  - Unknown operations fail closed.
- [x] Added CLI `daemon-status` probe.
- [x] Ran a live two-process Windows integration check: started `runwatch serve`, connected through the named pipe from a separate CLI process, and received `protocol_version=1`, `storage=sqlite-wal`, and the advertised capabilities.

Validation after R1b:

- `cargo fmt` — passed.
- `cargo check --all-targets` — passed; existing `russh v0.54.5` future-incompatibility warning remains.
- `cargo test --all-targets` — **11 passed, 0 failed**.
- `cargo build -p runwatch-cli` — passed.
- live named-pipe handshake — passed.

### R1c — daemon-only mutation + explicit Delivery repository — completed 2026-08-31

Landed slices:

- [x] Added versioned daemon IPC mutations for durable remote `submit_run_v2`, continuation rebind/session lease/delivery claim+ack, and scheduler `cancel_run`.
- [x] Added explicit Delivery rows with deterministic ids, attempt counters, claim expiry, retry/needs-rebind state and last-error persistence for Pi continuation.
- [x] IPC requests carry correlation ids; the Pi client validates response ids and applies bounded timeouts/cancellation.
- [x] Server and local probe now honor `RUNWATCH_ENDPOINT`, matching pi-runs' existing client override. Isolated acceptance/parallel test daemons can bind a unique named pipe/Unix socket without contending with the user's normal endpoint.
- [x] Added daemon-owned bounded `logs` observability. It can only tail the current durable Attempt's known stdout/stderr paths, clamps to 500 lines and 64 KiB per stream.
- [x] Added daemon-owned `artifacts` inventory for the current durable Attempt. It exposes only runwatch-known lifecycle paths (`script`, `stdout`, `stderr`, `terminal`, `receipt`) and does not become an arbitrary remote-file scanner.
- [x] `cancel_run` only records a durable `cancel_requested` event after `scancel`/`bkill` succeeds; it does **not** falsely mark the Run Cancelled before scheduler observation confirms terminal state.
- [x] CLI direct SSH `Ping`/`Refresh`/`Tick`/`Wait` now consume `ssh.alive_interval` and `ssh.cmd_timeout_sec`, matching daemon settings instead of hard-coded defaults.
- [x] Live two-process named-pipe smoke after the observability mutation slice: pi-runs discovered `logs` and `cancel_run` from the newly built daemon and capability-aware selection chose `runwatch` for both.
- [x] Added daemon `tick` and snapshot-only `wait_run` IPC operations. `wait_run` observes canonical SQLite state and does not start a second SSH/scheduler polling loop; the daemon remains the only lifecycle poller.
- [x] Migrated CLI `List`, `Refresh`, `Tick`, and `Wait` to local IPC. CLI `Refresh` requests a daemon tick and then reads the target Run; it no longer opens its own HostPool or mutates SQLite.
- [x] Added explicit compatibility capability `adopt_run_v1` and migrated the legacy CLI `Submit` command to it. The command is now documented as adopting/registering an existing scheduler Run; durable new scheduler submission remains `submit_run_v2`.
- [x] CLI Run-state paths now have no direct SQLite writer. `Status` remains read-only diagnostics and `Ping` remains a transport diagnostic rather than Run lifecycle mutation.
- [x] Replaced post-allocation `BufReader::lines()` size checking with a bounded frame reader. Requests larger than 256 KiB are rejected and the client connection is closed **before** the daemon accumulates an oversized line; an exact-limit frame remains valid.
- [x] Added daemon `daemon_status` + `set_paused` control state on the same shared `AtomicBool` used by `serve_loop`. A real isolated named-pipe smoke toggled `paused=false -> true -> false` against one daemon PID, proving Pause is daemon-owned rather than GUI-local.
- [x] Converted the GUI into a pure daemon observer/client: it no longer opens `RunStore`, no longer calls `serve_loop` or takes over polling when the daemon is absent, reads `daemon_status` + `list_runs` over IPC, derives terminal toasts from successive canonical snapshots, and sends Pause through `set_paused`. Daemon-offline UI is explicit and never creates a second scheduler owner.
- [x] The GUI clientized build passed `cargo check`; because the user's existing `target/debug/runwatch-gui.exe` was live/locked on Windows, full linking was verified safely in an isolated target directory instead of killing the running GUI.
- [x] Converted MCP Run tools to daemon IPC. `list_runs`, `get_run`, compatibility `submit_run` (`adopt_run_v1`), `tick`, short `wait_run`, `logs`, `artifacts`, and `cancel_run` no longer open `RunStore` or create their own `HostPool`; only read-only SSH alias listing remains local. Invalid runner names now fail closed rather than silently becoming Slurm.
- [x] Reworked legacy stdio MCP request handling so `tools/call` executes concurrently and responses flow through one async stdout writer. Tool results expose both bounded text and `structuredContent`; a short `wait_run` no longer serializes unrelated tool calls.
- [x] Real MCP concurrency smoke passed: while one queued Run waited for 3 seconds, `list_runs` returned first in **24 ms** and `wait_run` returned in **3076 ms**; both were successful and carried `structuredContent`.
- [x] That smoke exposed a real Windows local-IPC race: simultaneous clients could hit `ERROR_PIPE_BUSY (231)` before the daemon created the next named-pipe instance. The Rust local client now retries only that condition for at most one second; daemon-not-found and other errors still fail immediately. Regression coverage records the retry classification.
- [x] PowerShell test harnessing reconfirmed that its first redirected stdin line can carry a UTF-8 BOM. This was a harness artifact, not an MCP scheduler bug; production MCP framing remains normal UTF-8 JSONL.
- [x] Hardened the local IPC authorization boundary. Windows named pipes are created with `PIPE_REJECT_REMOTE_CLIENTS` and an explicit DACL granting pipe access only to the current user SID plus `SYSTEM`; focused tests prove the current-user SID is available and an ACL-restricted pipe instance can be created. Unix data directories are tightened to `0700` and the Unix-domain socket to `0600`.
- [x] Real same-user ACL smoke passed after hardening: an isolated daemon accepted `daemon-status` and `list` from a separate CLI process through the restricted named pipe.
- [x] Retired legacy shell-string callbacks without breaking historical data import. The v1 cleanup later removed `RunRecord.on_complete/on_success/on_failure/acked_at` from the current Rust model, JSON schema and MCP output schema entirely; serde still ignores those historical fields during the one-time `runs.jsonl` import. `runwatch-engine` contains no callback shell execution; hidden CLI `--on-success/--on-failure` flags fail explicitly before daemon/SSH access. Durable continuation uses explicit Delivery/outbox state only.
- [x] Final authority scan: CLI/GUI/MCP contain **zero** `RunStore` references; GUI/MCP contain **zero** `serve_loop` references; MCP contains **zero** `HostPool` references. CLI `Status` only computes the ledger path and does not open SQLite.
- [ ] Full MCP protocol modernization remains intentionally separate from R1c authority migration: the compatibility server still speaks the legacy `2024-11-05` handshake. Upgrade it coherently to the current 2026-07-28 MCP model/SDK and Tasks extension rather than changing only the advertised version.

### R1 round closeout validation — 2026-08-31

- [x] Removed stale README/GUI/engine text that still described `runs.jsonl` as the current ledger.
- [x] Final `cargo fmt -- --check` — passed.
- [x] Final `cargo check --all-targets` — passed; only the existing `russh v0.54.5` future-incompatibility warning remains.
- [x] Final `cargo test --all-targets` after R1c authority/security closeout — **45 passed, 0 failed, 1 real-Pi acceptance ignored by default**.

### R2a — observation fallback + per-host SSH isolation — completed 2026-08-31

Completed:

- [x] Replaced the old sentinel-only probe with a source-tagged probe contract. A configured sentinel is still the fast path, but if it is missing the **same remote command** now falls through to the scheduler instead of producing an unusable `missing -> Unknown` result.
- [x] Added strict POSIX shell quoting for remote sentinel paths and scheduler JobIDs used by lifecycle probes.
- [x] Made terminal parsing exact-token / structured-JSON based instead of substring matching, preventing log text such as `not-failed-yet` from becoming a false terminal transition.
- [x] Slurm observation now queries live state through `squeue` first and falls back to `sacct` accounting state after the job leaves the live queue.
- [x] Expanded Slurm mapping for `OUT_OF_MEMORY`, `PREEMPTED`, `NODE_FAIL`, `BOOT_FAIL`, `DEADLINE`, `SUSPENDED`, requeue/configuring states and related production outcomes.
- [x] LSF observation now uses `bjobs -a -o stat` and recognizes `POST_DONE`, `POST_ERR`, and `WAIT` in addition to the earlier live states.
- [x] Unknown/future scheduler output now preserves the last trusted Run execution state rather than overwriting it with `Unknown`. Full Observation rows remain a later model migration.
- [x] Reworked `HostPool` from one global session-map mutex held across remote command awaits to a short global map lock plus **per-host session locks**. A slow command on Host A no longer serializes Host B.
- [x] Added race-safe connection insertion: competing connects reuse the existing per-host session and close the duplicate candidate.
- [x] Added bounded SSH command execution timeout. The daemon now consumes `ssh.alive_interval` and `ssh.cmd_timeout_sec` from `config.yaml`; those settings were previously defined but ignored by `serve_loop`.

Validation:

- `cargo fmt -- --check` — passed after correcting one Rust literal-escaping typo found by the first formatter run.
- `cargo check --all-targets` — passed; only the existing `russh v0.54.5` future-incompatibility warning remains.
- `cargo test --all-targets` — **16 passed, 0 failed**.

### R2b — durable remote submission + fail-closed host keys — completed 2026-08-31

Completed:

- [x] Added first-class `RemoteWorkspaceRef`, `RunResources`, `SubmitRunSpec`, `RunAttemptRecord`, and `RunStatus::Submitting` foundations.
- [x] Added an atomic SQLite submission-intent transaction: `Run=Submitting`, Attempt 1 and a `submission_intent` Event are committed **before** wrapper deployment or scheduler submission.
- [x] Added daemon-owned remote submission service for `SSH(alias) × Slurm` and `SSH(alias) × LSF`.
- [x] Generated lifecycle wrappers live under `<remote cwd>/.runwatch/<run_id>/` and write structured terminal JSON through temp-file + atomic rename while preserving the scientific command exit code.
- [x] Added bounded, validated resource mapping for Slurm/LSF and deterministic scheduler job names.
- [x] Added a remote atomic `submission.receipt` guard. If the scheduler accepted a job but the SSH response is lost, retrying the **same run_id** first consumes the receipt instead of blindly submitting a duplicate job.
- [x] Definite deploy/scheduler failures persist `submission_failed`; transport ambiguity persists `submission_ambiguous` while keeping the Run in `Submitting` for safe same-ID retry.
- [x] Added versioned IPC capability `submit_run_v2`; the daemon IPC now shares the daemon-owned `Arc<RunStore>` and `Arc<HostPool>` instead of constructing separate control-plane instances.
- [x] Added a bounded 256 KiB request contract for local IPC; the later R1c framing hardening now rejects oversized requests before the daemon buffers a complete over-limit line.
- [x] Replaced the old accept-all SSH server-key handler with fail-closed `russh::keys::known_hosts::check_known_hosts()` validation against the user's standard `~/.ssh/known_hosts`, including ProxyJump hops.
- [x] Propagated the new `Submitting` state through GUI/status projections.

Validation:

- `cargo check --all-targets` — passed after compiler-guided propagation of the new `Submitting` state and SSH handler type.
- `cargo test --all-targets` — **22 passed, 0 failed**.
- Existing `russh v0.54.5` future-incompatibility warning remains.

### R2c — effective OpenSSH semantics + scheduler history/observability — in progress

Completed in this slice/current worktree:

- [x] Host-key checking is fail-closed against the user's standard `~/.ssh/known_hosts`, including jump hops; the old accept-all handler is gone.
- [x] SSH command execution is bounded by `ssh.cmd_timeout_sec`; a timeout invalidates only that Host session.
- [x] Session synchronization is per Host rather than one global mutex across command awaits.
- [x] Daemon-owned bounded stdout/stderr tail is exposed as IPC `logs` and pi-runs can consume it.
- [x] Daemon-owned Slurm/LSF cancel request is exposed as IPC `cancel_run` and pi-runs can consume it.
- [x] LSF observation now falls back from `bjobs -a` to `bhist -l` when the job has aged out of recent-job visibility; historical `Done successfully`/post-job success map to DONE and exit/post-job-failure events map to EXIT.
- [x] Declared Host aliases are now resolved through `ssh -G <alias>` before russh connects, so effective OpenSSH HostName/User/Port/IdentityFile/ProxyJump values (including `Host *`/Match-style effective settings) drive the persistent transport instead of the partial parser's value precedence.
- [x] Effective `IdentityFile` is now plural. runwatch tries configured unencrypted private keys in OpenSSH effective order rather than silently keeping only one IdentityFile.
- [x] Effective `UserKnownHostsFile` and `HostKeyAlias` are now honored for host-key verification. This fixed a real Windows incompatibility in russh 0.54.5's default known-hosts helper, which searched `%HOME%/ssh/known_hosts` while OpenSSH/cap00 uses `%HOME%/.ssh/known_hosts`. Verification remains fail-closed and uses `check_known_hosts_path` over the effective OpenSSH paths.
- [x] Fixed SSH channel completion ordering: `Eof` no longer terminates result collection before a later `exit-status`; collection ends on `Close`. Without this, successful remote commands could return correct stdout but `code=None` and be falsely treated as failures.
- [x] Added typed first-class `ObservationRecord` snapshots keyed by `(run_id, attempt_no)`: `source={local_process,sentinel,scheduler,compatibility,transport}`, `health={fresh,probe_error,unreachable}`, `observed_at`, last trusted `execution_status`, bounded raw state, reason and remote command exit code. Every probe refreshes the snapshot timestamp; `observation_changed` Events are appended only on semantic changes so normal polling does not flood history.
- [x] Poller semantics now enforce `execution != observation`: local-process probe failures, SSH transport failures, non-zero/missing remote probe exit status and unrecognized scheduler state update Observation health/reason while preserving the last trusted Run execution status. A successful recognized probe marks `fresh` and may advance execution state.
- [x] `get_run` now returns the current Attempt observation as a backwards-compatible sidecar, `get_observation` is an explicit IPC capability, and `list_runs` includes an observations sidecar for clients that want overview health without changing the existing `runs` array shape.
- [x] Real isolated smoke passed: Run `obs_smoke_0901` began `running` against a deliberately missing SSH alias; daemon tick reported the transport error but Run stayed **running** while Observation became `health=unreachable`, `source=transport`, with the concrete reason. The real pi-runs client returned the same sidecar and rendered `Runs 1 running · 1 probe issue`.
- [x] Added resident-loop adaptive polling without weakening explicit control operations. CLI/MCP `tick` still force-probes every live Run; only `serve_loop` uses `tick_due`. A Run with no Observation is always probed immediately. Remote Queued/Submitting remains at the base interval, remote Running uses 2× base, `probe_error` uses at least 2×, and `unreachable` uses at least 4× with a 5-minute cap; Local Process stays at the base interval because local identity checks do not load an HPC login node.
- [x] Adaptive due decisions are driven by persisted `Observation.observed_at`, so daemon restart does not reset every Run into an immediate high-frequency poll storm. Three focused policy tests cover queued/local conservatism, running/unreachable backoff + cap, and exact due boundaries.
- [x] Added per-Host **Slurm v2 scheduler batching** for the resident `tick_due` path. Only Runs whose durable Attempt matches `runner=slurm`, Attempt/Run JobID and runwatch-owned terminal path are eligible; legacy adopted Runs and singletons stay on the existing per-Run probe.
- [x] A batch uses one SSH exec for the Host, checks each runwatch-owned one-line terminal sentinel, then performs exactly one `squeue` query and one `sacct` query for the complete JobID set. Interpretation precedence is terminal → live `squeue` → accounting `sacct`; unknown/missing states become `probe_error` while preserving execution state. If the batch transport fails, all members get `transport/unreachable` and are not immediately retried individually in the same tick.
- [x] Added parser/command/eligibility tests plus a default-ignored real Slurm acceptance. Explicit cap00→gm00 run passed: two 30-second `runwatch-batch-smoke` Slurm jobs were submitted, one generated batch probe recognized both as queued/running, and cleanup removed both; a follow-up `squeue -n runwatch-batch-smoke` was empty.
- [x] Extended per-Host batching to **LSF v2 active/recent jobs** without weakening historical correctness. Batchable LSF Runs use one SSH exec with runwatch-owned sentinel fast paths plus one `bjobs -a -noheader -o 'jobid stat'` query for the group. Recognized terminal/active rows are handled in batch; a missing/unrecognized `bjobs` member is intentionally left unhandled so the same tick falls through to the existing per-Run `bjobs -> bhist` aged-out-history fallback.
- [x] LSF batch-level SSH/command failures update group Observation health and suppress same-tick retry storms, while scheduler-row absence is **not** treated as batch failure. Parser/command/eligibility tests cover the fallback boundary.
- [x] Real cap00→`gyz-mn02` IBM LSF 10.1 acceptance passed: two 30-second `runwatch-lsf-batch-smoke` jobs were submitted, one generated batch `bjobs` probe recognized both, cleanup removed them, and a follow-up `bjobs -J runwatch-lsf-batch-smoke` returned no live job.
- [x] Extended host-key trust parity to effective `GlobalKnownHostsFile` in addition to `UserKnownHostsFile`/HostKeyAlias. OpenSSH's Windows `__PROGRAMDATA__` placeholder is expanded to `%PROGRAMDATA%`; global + user files are de-duplicated, missing default files are ignored like OpenSSH, while any existing known-hosts file remains fail-closed on parse/mismatch.
- [x] Hardened ProxyJump parsing without changing the public `proxy_jump: Vec<String>` UI/MCP wire. Transport now parses `[user@]host[:port]`, bracketed IPv6 with optional port, and raw unbracketed IPv6 host-only forms. A jump alias uses its own effective OpenSSH config; a raw host is resolved via its own `ssh -G` instead of incorrectly inheriting the target's IdentityFiles. Explicit user/port override the effective hop, and the explicit chain is then carried with russh `direct-tcpip`.
- [x] Added bounded SSH-agent fallback after existing unencrypted effective IdentityFiles. Windows tries the standard OpenSSH agent named pipe and then Pageant; Unix uses `SSH_AUTH_SOCK`. Agent connection and authentication phases are independently capped at 3 seconds. This supports encrypted/private identities already loaded into an agent without introducing another credential database.
- [x] Final real connection regressions passed with the rebuilt binary: direct `runwatch ping gm00 -> mn01` and ProxyJump `runwatch ping gyz-mn02 -> mn02`. `runwatch-ssh` focused tests are 5/5. cap00's current `ssh-add -l` reports **no identities**, so agent-backed authentication is implemented but cannot honestly be marked as a real-key acceptance on this machine yet.
- [x] Reworked alias discovery to recurse OpenSSH `Include` files instead of pretending the discovery parser is also a connection-config parser. `parse_ssh_config_at` now only collects exact `Host` tokens; relative/absolute/`~` Include patterns are expanded with `glob 0.3.4`, nested includes recurse, canonical-path visitation breaks include cycles, and wildcard/negated Host patterns are intentionally excluded. Every discovered alias is still resolved through `ssh -G`, so OpenSSH remains the authority for actual connection semantics.
- [x] Recursive Include focused acceptance passed with a synthetic `config.d/*.conf -> nested.conf -> ../config` cycle: discovered aliases were exactly `cluster, deep, direct, exact-two`, with wildcard/negated/bracket patterns excluded. Full workspace remained **74 passed / 0 failed / 5 ignored** and freshly rebuilt `runwatch hosts` still resolved the current **52 configured aliases**, including existing ProxyJump chains.
- [x] Added exact `IdentitiesOnly=yes` unattended semantics. Effective `ssh -G` now carries `identitiesonly`; direct unencrypted IdentityFiles are tried first, while agent/Pageant fallback is filtered to public keys derived from those configured identities. Encrypted private keys are never prompted for or stored by runwatchd: a matching public identity is taken from the key's readable public form or adjacent `.pub`, and the private key must already be loaded into OpenSSH agent/Pageant. If no matching public identity is available, authentication fails closed with explicit remediation.
- [x] Frozen the host-key trust policy in code: runwatch intentionally does **not** inherit trust-relaxing client modes such as `StrictHostKeyChecking=no/accept-new`. `KnownHostsHandler` accepts only keys already present in effective Global/UserKnownHostsFile. A regression test proves that a valid server public key with no existing known-hosts entry is rejected.
- [x] Final SSH-focused matrix is **7/7** and full workspace is **76 passed / 0 failed / 5 ignored**. Rebuilt direct/ProxyJump pings remained green (`gm00 -> mn01`, `gyz-mn02 -> mn02`). cap00's current `ssh-add -l` reports no identities, so a real agent-loaded encrypted-key acceptance remains environment-gated rather than falsely reported as passed.

R2c is complete. A real agent-loaded encrypted-key smoke should be added when cap00 has a test identity in OpenSSH agent/Pageant, but the unattended policy and filtering implementation are no longer missing.


### R2d — Windows Local × Process durable lifecycle — lifecycle acceptance passed 2026-09-01

Completed:

- [x] Added first-class `RunnerKind::Process` to daemon-owned `submit_run_v2`. Local submission commits `Run=Submitting`, Attempt and optional ContinuationBinding before process launch.
- [x] Replaced the old Pi/PowerShell `Start-Job` durability assumption with a detached Windows process wrapper under `<cwd>/.runwatch/<run_id>/`. The wrapper writes `started.json`, waits for a durable `armed` boundary, executes the science command, captures stdout/stderr and atomically writes `terminal.json`.
- [x] Local handles encode `(PID + Windows process creation time)` as `local:<pid>:<creation-time>`, so daemon restart cannot confuse a recycled PID with the original science process. Recovery consults durable Attempt metadata, `submission.receipt`, then `started.json`.
- [x] Local logs, artifact inventory, observation and cancellation use the same Run surface. Cancellation is persisted as a request first; final terminal state is still observation-driven.
- [x] Local process creation requires Windows Job breakaway. If the host Job Object rejects breakaway (`ERROR_ACCESS_DENIED`), runwatch now fails closed with an explicit explanation instead of launching a child that could disappear with the daemon. The error points operators to the resident `runwatch supervise` / Task Scheduler path.
- [x] A direct WebCodex-hosted daemon smoke intentionally hit this fail-closed boundary (`Access is denied`, os error 5), demonstrating that a restricted parent Job cannot silently downgrade durability.
- [x] Representative resident-path acceptance then passed through a disposable real Task Scheduler supervisor: supervisor PID **83580** launched daemon PID **16496**; the real pi-runs runwatch client submitted Run `local_durable_0901` with stable process handle **`local:40660:01dd39ae4307fb57`**; daemon PID 16496 was terminated while the 8-second science command was running; supervisor created daemon PID **86768**; the detached science process still wrote `result.txt=local-process-ok`; the reopened canonical store converged to `succeeded`.
- [x] The same pi-runs client read `stdout=science-finished`, empty stderr and lifecycle artifacts `script/stdout/stderr/terminal/receipt`. Disposable task, supervisor/daemon processes and temporary tree all returned to zero after cleanup.

Acceptance boundary: this proves the **Local Process execution lifecycle** survives daemon death on the supported resident Windows path and is reachable through the default pi-runs runwatch client. Pi live/offline exact-session continuation is the same generic Delivery pipeline already accepted independently; one combined local-Process + real-provider Pi continuation Run remains useful release-hardening, not an unresolved process-durability mechanism.

### R4/R6 — durable Pi live continuation + branch-safe rebind — implementation landed 2026-08-31

Completed:

- [x] Added `ContinuationBinding` to `SubmitRunSpec` and persisted it in the **same SQLite transaction** as `Run=Submitting`, Attempt 1 and the `submission_intent` Event.
- [x] Binding captures the agent kind, Pi session id/session file, origin leaf, project root and exact `RemoteWorkspaceRef`. Run compatibility projections also carry session/agent/project fields.
- [x] Added daemon-side `AgentSessionRegistration` leases with owner-instance identity and bounded TTL. A second live Pi instance cannot acquire the same `agent_kind + session_id` while the first lease is valid.
- [x] Added deterministic terminal Delivery outbox rows (`<run_id>:a<attempt>:terminal`) and `continuation_pending` Events. Re-ticking an already-terminal Run recreates a missing Delivery if needed, so daemon restart does not lose completion work.
- [x] Added pull-based live delivery operations: `register_agent_session`, `release_agent_session`, `claim_deliveries`, `delivery_status`, and `ack_delivery`.
- [x] Claims are bounded, lease-protected, attempt-counted and expire back to retryable pending state. Delivery outcomes support `delivered`, `retry`, and `needs_rebind`.
- [x] Added explicit `rebind_continuation`: it atomically replaces the Run binding, writes a `continuation_rebound` Event, and moves that Run's `needs_rebind` deliveries back to pending.
- [x] Added IPC capabilities for all live-bridge/rebind operations; unknown operations continue to fail closed.
- [x] Cross-project live named-pipe smoke passed without submitting compute:
  - Pi client discovered `submit_run_v2`, lease, delivery-status/claim/ack, and rebind capabilities.
  - first owner acquired the lease;
  - delivery status returned all-zero clean state;
  - second owner for the same Pi session was rejected;
  - after release, the second owner acquired and released the lease successfully.

Validation:

- `cargo fmt -- --check` — passed.
- `cargo check --all-targets` — passed; only the existing `russh v0.54.5` future-incompatibility warning remains.
- `cargo test --all-targets` — **26 passed, 0 failed**.
- `cargo build -p runwatch-cli` — passed before the live named-pipe smoke.

#### R3/R4 closeout — production backend + dedicated live Pi gate — completed 2026-08-31

- [x] pi-runs `PI_RUNS_BACKEND=auto` no longer silently falls back to `~/.pi/runs` when runwatchd is offline or lacks a capability. Default tools now fail closed against the single durable authority; `PI_RUNS_BACKEND=legacy` is explicit migration compatibility only.
- [x] This intentionally removes default local/PowerShell `Start-Job` behavior until runwatch gains first-class Local × Process; a known non-durable compatibility path is not presented as production continuation.
- [x] pi-runs backend regression after the change: **32 passed, 0 failed, 1 real-Pi live gate skipped by default**; real Pi RPC with a missing runwatch endpoint emitted `Runs unavailable` and exited normally.
- [x] Dedicated real Pi live-terminal gate passed: an isolated fake runwatch derived the binding from Pi's real registered session, delivered one synthetic terminal completion, Pi persisted `runwatch/completion`, emitted `agent_start`, and the live bridge acked `delivered`. Explicit gate: **1 passed, 0 failed** in about 2.3s.

## R5/R6 — offline exact-session continuation + branch-safe recovery — success and `/tree` rebind acceptance passed; combined HPC/crash gates next

Implemented:

- [x] Terminal Pi Deliveries with no fresh live lease become reservable offline `AgentInvocation` records after a bounded grace period.
- [x] Offline dispatch shares the same exclusive `agent_kind + session_id` execution lease used by live Pi, preventing concurrent writers to one Pi session.
- [x] Worker launch uses the recorded exact session file/project root and pi-runs adapter through Pi RPC mode; Windows launch uses `CREATE_NO_WINDOW`.
- [x] The exact Delivery is injected through a bounded base64 bootstrap environment value rather than a fabricated user prompt.
- [x] pi-runs reuses the same project-trust and branch-lineage checks. Untrusted or mismatched resume is blocked as `needs_rebind`; no implicit approve/trust bypass exists.
- [x] Offline completion is not acknowledged merely because Pi spawned. pi-runs waits for its injected agent turn to reach `agent_settled`, then durably acks and shuts down the RPC worker.
- [x] Unacked/failed worker exit is requeued by the durable invocation/delivery state machine; launch count/runtime are bounded.
- [x] Fixed a Windows launcher blocker discovered by real cap00 testing: `std::process::Command::new("pi")` returned `program not found` because Pi is exposed by Volta shell shims (`pi.cmd` / extensionless shell script), not a native `pi.exe`. Offline dispatch now resolves a native `pi.exe` first or a native `volta.exe` launcher with prefix `run pi`; it deliberately refuses to rely on `.cmd/.bat` shell shims for unattended session/project paths. `RUNWATCH_PI_EXECUTABLE` remains available for an explicit native launcher override.
- [x] Added Windows launcher regression tests: native `pi.exe` takes priority, Volta is a safe native fallback, and a lone `pi.cmd` is not treated as a native executable.
- [x] Added `RUNWATCH_DATA_DIR` override so fault/acceptance daemons can use an isolated SQLite/lock/heartbeat directory without touching the user's normal `~/.runwatch` state.

Partial real Pi 0.84.4 acceptance completed on cap00:

- [x] Created an isolated Pi RPC session in a temporary `--session-dir`, ran a minimal agent turn, observed the official `agent_settled` event, and obtained the exact `sessionFile/sessionId` through `get_state`.
- [x] Started a second Pi RPC process with `--session <exact sessionFile>` and verified it reopened the same session id/file with the expected message count rather than creating a new session.
- [x] Verified cap00's native `volta.exe run pi --version` launch path and confirmed Pi 0.84.4. The full daemon offline dispatcher still needs end-to-end Delivery bootstrap acceptance below.

Real gm00 Slurm + offline-failure acceptance completed on an isolated daemon/store:

- [x] Started runwatch with `RUNWATCH_DATA_DIR=%TEMP%\\runwatch-r5-acceptance`, leaving the normal user store untouched.
- [x] Submitted a real minimal Slurm Run through `submit_run_v2`: `r5-smoke-20260831-1016`, JobID **31732**, `/tmp`, `echo RUNWATCH_R5_OK`, 1 CPU, 1 minute limit. The daemon persisted Run/Attempt/ContinuationBinding before scheduler submission.
- [x] The daemon observed the real scheduler path `Queued -> Succeeded` and created deterministic Delivery `r5-smoke-20260831-1016:a1:terminal`.
- [x] With no live Pi lease, runwatch reserved and launched an offline AgentInvocation, reopened the exact Pi session and pi-runs persisted a `runwatch/completion` custom message containing the correct Run, JobID, workspace and invocation id.
- [x] The continuation model then encountered a real provider-side HTTP 429. The Delivery remained unacknowledged while Pi was still in its retry lifecycle; it was **not** falsely marked delivered.
- [x] After deliberately terminating only the isolated offline worker to simulate worker loss, runwatch observed the process exit and durably moved Delivery to `retrying`, Invocation to `failed`, recorded `agent_invocation_exit`, and preserved the already-succeeded Run. This validates the crash/retry path across a real Slurm completion.
- [x] The isolated acceptance daemon was allowed to terminate after the test window so it could not keep relaunching the retryable test Delivery.

Provider-successful exact-session completion acceptance also passed on cap00 without touching the normal runwatch store:

- [x] Added `runwatch-core/examples/r5_offline_seed.rs`, which refuses to run unless `RUNWATCH_DATA_DIR` is set and creates only a synthetic terminal Run/Attempt/Delivery in that isolated SQLite store.
- [x] Created a fresh real Pi 0.84.4 session under a temporary `--session-dir` using `localCPA/gpt-5.6-luna`, captured its exact persisted session file/id and origin leaf, then fully exited the seed Pi process.
- [x] The first full daemon rerun exposed a real synchronous lifecycle race: `triggerTurn` could emit `agent_start` before `pi.sendMessage()` returned, while pi-runs marked `offlineBootstrapSent` only afterwards. The model turn completed but Delivery remained `delivering` forever. pi-runs now arms that lifecycle gate **before** `triggerTurn` and resets it only if injection itself throws.
- [x] Repeating the same isolated gate after the fix produced `pending -> delivering -> delivered`, AgentInvocation `running -> completed`, and final session lease count `0`.
- [x] The durable event sequence includes `continuation_pending -> continuation_delivered -> agent_invocation_exit`; the exact resumed JSONL contains exactly one `runwatch/completion`, and the completion turn ended with assistant `stop` on `gpt-5.6-luna` without resubmitting the completed Run.
- [x] The synthetic workspace intentionally used a nonexistent Host alias, so `runs_logs` / `ssh_activate` failed inside the resumed research turn as expected; that fixture noise did not affect continuation settlement and demonstrates that workspace-access failure is not confused with Delivery transport failure.
- [x] Added a reusable default-ignored `real_pi_offline_continuation_acceptance` test. With explicit isolated `RUNWATCH_DATA_DIR`, unique `RUNWATCH_ENDPOINT`, real Pi session file/id/origin leaf, adapter path and project root, it starts an isolated IPC server, creates a synthetic terminal Delivery, launches the actual offline Pi RPC worker and asserts durable `delivered`. A cap00 Pi 0.84.4 run using a normal Pi-created session and `cliproxyapi/gpt-5.4-mini` passed in **63.18 s**.
- [x] A second real Slurm success attempt (JobID **31733**) verified deeper three-project integration: offline Pi resumed, `runs_status`/`runs_logs` came from runwatch, and the automatically loaded `pi-ssh-tools` proceeded with `ssh_read`/`ssh_bash` on `gm00:/tmp`. This attempt exposed a migration seam where `runs_harvest` fell back to legacy because runwatch lacked `artifacts`; the new daemon `artifacts` capability closes that seam before the final success rerun.

Real same-session `/tree` divergence + explicit rebind acceptance now also passes on cap00:

- [x] Used Pi 0.84.4's own `SessionManager.branch()` + append-only entry API to move the active leaf inside the **same session JSONL**, proving a real `/tree`-style divergence rather than a new-session `/fork`.
- [x] A Delivery bound to the abandoned origin leaf was rejected as `needs_rebind`; the offline Invocation became `blocked`, the session lease returned to zero, and the wrong branch received **zero** `runwatch/completion` messages.
- [x] That gate exposed an early-shutdown handshake bug: branch/trust rejection can happen in `session_start` before any agent turn, while Pi RPC waits on stdin. runwatch now keeps RPC stdin open only while Delivery=`delivering` and closes it after any durable terminal delivery outcome, allowing `ctx.shutdown()` to end blocked workers without `process.exit()`.
- [x] The first real `runs_rebind` recovery exposed a durable snapshot bug: `continuation_bindings` changed but `deliveries.payload_json.binding` still held the abandoned leaf, so the requeued Delivery immediately blocked again. Rebind now atomically rewrites every unclaimed pending/retrying/needs_rebind Delivery snapshot to the new binding and refuses rebind while any Delivery is already `delivering`.
- [x] Repeating the same real blocked Run after the fix succeeded: Delivery attempts reached 3, the binding table and Delivery snapshot both used new origin leaf `0a5a03ee` instead of old `d277fda1`, exactly one completion message was delivered, lease count returned to zero, Pi ended with `stop`, and the same Delivery became `delivered`.

Combined real HPC/provider-success acceptance is now green:

- [x] Started a fresh isolated runwatch daemon/store and a fresh real Pi 0.84.4 session, then submitted Run `r5_combined_14bed66b438b` through the actual `runs_submit` tool to `gm00:/tmp`; Slurm assigned JobID **31734**.
- [x] The initial Pi process reached `agent_settled`, persisted its exact session file, then exited completely. The daemon independently observed the scheduler Run as `succeeded`, created the pending Delivery and later launched the exact-session offline Pi worker.
- [x] The real Delivery transitioned `pending -> delivering -> delivered` on the first attempt; AgentInvocation transitioned `running -> completed`; the exact-session lease returned to zero; durable events were `submission_intent -> scheduler_submitted -> continuation_pending -> continuation_delivered -> agent_invocation_exit`.
- [x] The same resumed Pi JSONL received exactly one `runwatch/completion`. After that completion it actually called `runs_status`, `runs_logs`, `runs_harvest`, `ssh_activate`, `ssh_read`, and `ssh_bash` against the recorded `gm00:/tmp` workspace, then ended with assistant `stop`.
- [x] The scientific smoke command wrote `/tmp/runwatch-combined-c4934ac988.txt` containing the Run-specific success marker, so this single gate covers durable scheduler submission, terminal observation, offline exact-session restoration, runwatch observability/artifact inventory, explicit pi-ssh-tools workspace activation and real remote result inspection without a human “continue”.

Acceptance still required:

- [x] Clean **combined real Slurm + provider-success** acceptance passed with gm00 JobID **31734**, exact-session offline relaunch, successful settlement, runwatch status/logs/artifacts and explicit pi-ssh-tools result inspection in one Run.
- [x] Real same-session `/tree` divergence + explicit `runs_rebind` recovery passed. Pi RPC `/fork` was also examined and correctly creates a new session file/id, so exact-session delivery to the original file is not a lineage violation.
- [x] Daemon/process crash correctness matrix passed across deterministic reservation/ack windows plus real Windows daemon kill/restart. Multi-hour soak remains a durability/performance release test, not an unresolved state-machine window.

### R7a — lease-aware daemon/process crash recovery — implementation landed 2026-08-31

Completed:

- [x] Added `offline_invocation_is_owned(invocation_id, delivery_id, owner_instance_id)`: an offline worker is authoritative only while its Invocation is `starting|running`, its exact Delivery is still `delivering`, and the same owner holds an unexpired Pi session lease.
- [x] Added IPC capability `verify_offline_invocation`; pi-runs now revalidates that ownership after lease registration and **before** injecting an offline completion. A late worker whose old invocation was reconciled/requeued shuts down without injecting stale completion text.
- [x] Added `reconcile_orphaned_agent_invocations`: only Invocation rows in `starting|running` with no active same-owner lease are reconciled. `delivering` becomes retrying, delivered becomes completed without requeue, needs_rebind becomes blocked, and stale matching leases are removed with an `agent_invocation_reconciled` event.
- [x] Daemon startup waits a 30-second orphan reconnect grace before reconciliation. A surviving old Pi worker refreshes its same owner lease every 10 seconds and therefore remains authoritative; recovery only takes over when that lease truly stays expired.
- [x] Recovery is independent of scheduler tick success and then feeds the normal bounded offline dispatcher.
- [x] Unit coverage now includes reserve-before-spawn (`starting` orphan), spawn-before-ack (`running` orphan), active worker non-preemption, exact ownership verification, and ack-before-process-exit (`delivered` orphan -> completed without duplicate Delivery).

### R7b — session-side continuation idempotency — acceptance passed 2026-08-31

Completed across pi-runs + runwatch:

- [x] Successful offline `agent_settled` now persists a branch-local Pi custom session entry `runwatch/completion-settled` **before** attempting the runwatch `delivered` ack. The receipt is extension state only and is not sent back into LLM context.
- [x] A restarted worker inspects the exact current branch before injection. If the settlement receipt already exists, or the original `runwatch/completion` is already followed by a persisted terminal assistant `stop`, it repairs/acks the durable Delivery without creating a second completion turn.
- [x] If the original completion exists but the prior turn is incomplete or failed, pi-runs sends `runwatch/completion-recovery` to continue the existing context rather than duplicating the original completion message.
- [x] Real isolated replay acceptance passed: after a successful exact-session continuation, the SQLite Delivery was deliberately rewound to `retrying` to model “Pi settlement receipt persisted but runwatch ack was lost”. A second offline Invocation raised Delivery attempts from 1 to 2 and restored `delivered`, while the Pi session remained at **1 completion message, 0 recovery messages, 1 settlement receipt**, both Invocations completed and the lease returned to zero.

### R7c — real Windows daemon kill/restart + live HPC continuation — passed 2026-08-31

Two real process-level gates close the restart matrix:

- [x] **Injected-before-settled daemon kill:** after a real headless Pi worker had persisted the original `runwatch/completion` but before settlement, daemon PID 83840 was force-killed. The old worker exited with the parent stdin pipe, its Invocation was reconciled to failed, a second Invocation took Delivery attempt 2, and the exact session finished `delivered` with **1 original completion + 1 recovery message + 1 settlement receipt**, no lease and no surviving worker process.
- [x] **Real remote scheduler restart:** Run `r7_hpc_a1640dc494c6`, gm00 Slurm JobID **31735**, was confirmed `running` when daemon PID 81212 was force-killed. After a 7-second local daemon outage, the same SQLite store/poller resumed; the remote job continued independently, became `succeeded`, then exact-session offline continuation delivered on attempt 1 and the Invocation completed with lease count 0.
- [x] The resumed Job 31735 session contained exactly one completion + one settlement receipt and actually executed `runs_status`, `runs_logs`, `ssh_activate`, `ssh_read`, and `ssh_bash` against `/tmp/runwatch-r7-7c0600a7e9.txt`, then ended with assistant `stop`.
- [x] Combined with repository fault tests for reserve-before-spawn (`starting` orphan), spawn-before-ack (`running` orphan), active-worker non-preemption, and ack-before-process-exit (`delivered` orphan -> completed), every identified durable crash window now has deterministic or real-process evidence.

R7 functional correctness is therefore closed. A multi-hour scheduler/daemon soak should still be run before a production release, but it is no longer masking an unknown recovery transition.

### R8a — Windows resident Task Scheduler service + GUI/service split — implementation landed 2026-08-31

Completed:

- [x] Fixed the old autostart semantic bug: the Startup-folder `.cmd` launched only `runwatch-gui.exe`, but the GUI is now a pure daemon client and therefore could not guarantee a resident scheduler owner after logon.
- [x] Split GUI autostart from daemon service lifecycle. GUI Startup now uses `runwatch-gui.cmd`; the old `runwatch.cmd` is still recognized/removed for migration compatibility. CLI `autostart` now manages the daemon Task Scheduler task, while the GUI exposes separate `Keep runwatchd running` and `Start GUI with Windows` toggles.
- [x] Established the current-user Task Scheduler integration (`InteractiveToken`, `LeastPrivilege`, `StartWhenAvailable`, `MultipleInstancesPolicy=IgnoreNew`, no battery stop, no execution time limit). The original direct `runwatch.exe serve` + `RestartOnFailure` assumption was later **superseded by R8c real fault injection**; Windows did not restart an action that had started successfully and later exited nonzero.
- [x] Registration uses native `schtasks.exe` with no PowerShell registration script and no console window. The final task action is `runwatch.exe supervise --interval 20`; installation starts the supervisor immediately, and the supervisor waits rather than competing when another fresh daemon already owns the ServeLock.
- [x] A real cap00 Task Scheduler smoke caught a Windows-specific encoding failure (`unable to switch the encoding`) that unit/schema checks could not see. Task XML is now written as UTF-16LE with BOM and `encoding="UTF-16"`.
- [x] The registration smoke now round-trips the final resident contract: `TimeTrigger` with `PT1M` reconciliation, `MultipleInstancesPolicy=IgnoreNew`, `supervise --interval 20`, and no misleading `RestartOnFailure` dependency. Task Scheduler may normalize default XML fields on readback, so the gate checks semantic fields rather than byte-for-byte XML.
- [x] `runwatch autostart` read-only status passed on cap00 and reported `daemon=disabled task=runwatchd  gui_autostart=disabled`, proving the acceptance did not silently install a persistent production task.
- [x] GUI service-menu changes fully linked in an isolated target directory so the user's existing GUI process did not need to be terminated; the isolated target directory was cleaned afterwards.

### R8c — Windows supervisor + real resident fault recovery — completed 2026-08-31

R8c deliberately faulted the real installed-task lifecycle and changed the architecture based on observed Windows behavior:

- [x] **Negative acceptance:** the original direct scheduled `serve` process really started (first observed daemon PID 63656), was force-killed, and the task returned `Last Result=1`; after 90 seconds Task Scheduler was `Ready` with no second daemon PID. This disproved the assumption that `<RestartOnFailure>` is a general watchdog for an action that started successfully and later died.
- [x] **Windows path bug found and fixed:** canonicalized paths such as `\\?\E:\...\runwatch.exe` and `\\?\C:\Windows\System32\cmd.exe` failed when consumed by Task Scheduler/cmd. Task action paths now strip Win32 verbatim prefixes (`\\?\C:\... -> C:\...`, `\\?\UNC\... -> \\...`) with unit and real registration coverage.
- [x] Added `runwatch supervise --interval N`. The supervisor owns only process lifecycle — never `RunStore`, SSH or scheduler state — and has its own exclusive lock/pid file. It waits behind a fresh pre-existing daemon instead of competing for the ServeLock.
- [x] Managed `serve` children are placed in a Windows Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so ending/crashing the supervisor cannot leave an orphan daemon. Child restart uses bounded `1/2/5/10/30s` backoff and resets after a stable run.
- [x] Task Scheduler is now a coarse **reconciler**, not the daemon watchdog: a one-minute `TimeTrigger` repeatedly requests `supervise`, while `IgnoreNew` suppresses duplicates; the logon trigger and explicit install-time `/Run` provide immediate/session startup. If the supervisor dies, the next reconciliation starts a new one.
- [x] **Real child-crash acceptance:** Task Scheduler started the disposable supervisor; force-killing `serve` PID **74640** produced ready `serve` PID **38208** on the same isolated IPC endpoint within seconds.
- [x] **Real supervisor-crash acceptance:** force-killing supervisor PID **55056** caused Task Scheduler reconciliation to create supervisor PID **11544**; the old child PID **38208** disappeared through the Job Object and the new supervisor created ready daemon PID **85204**. The full two-layer acceptance passed in **62.45s**.
- [x] Earlier acceptance also independently demonstrated the Job Object boundary: killing an identified orphan supervisor PID 17968 left child PID 82076 already gone, confirming child teardown on supervisor handle closure.
- [x] Disposable `runwatch-service-smoke-*` / `runwatch-task-smoke-*` tasks, R8c processes and temporary directories were explicitly checked and returned to zero after cleanup.

### R8b — MCP 2026-07-28 SDK/Tasks/typed-output modernization — completed 2026-08-31

Completed:

- [x] Deleted the handwritten stdio JSON-RPC MCP server and migrated `runwatch-mcp` to the official Rust SDK `rmcp 3.1.4` with stdio transport, `ToolRouter`, `TaskManager`, SDK protocol negotiation, and SDK-owned legacy initialize compatibility.
- [x] The SDK server advertises MCP `2026-07-28` through `server/discover` while still negotiating the legacy `2024-11-05 initialize` flow for older clients; no manual protocol-version string shim remains.
- [x] Preserved the nine public tools (`list_runs`, `get_run`, `list_hosts`, compatibility `submit_run`, `tick`, `wait_run`, `logs`, `artifacts`, `cancel_run`) while keeping runwatchd as the sole Run/scheduler authority.
- [x] Added current structured tool results with both human-readable text and `structuredContent`, plus explicit typed `outputSchema` for **9/9** tools. Success schemas exactly follow the existing daemon wire shapes and are unioned with `{error:string}` so structured tool errors also conform.
- [x] Added MCP tool annotations: observation/list/log/artifact tools are marked read-only where appropriate; scheduler cancellation is destructive; state-refresh/submission remain non-read-only without pretending to be destructive.
- [x] Implemented bounded long-wait semantics: `timeout_sec <= 60` remains a normal synchronous tool; waits above 60 seconds require the Tasks extension and are capped at 86400 seconds. Without Tasks, long wait fails closed immediately instead of silently truncating or blocking the stdio control plane.
- [x] MCP Task cancellation cancels only the **wait handle**. It never sends `cancel_run` and never changes durable Run state; scientific cancellation remains an explicit separate tool.
- [x] Real raw-stdio 2026-07-28 smoke passed: Discovery advertised 2026 + Tasks, all 9 tools were listed, a 61-second wait without Tasks failed closed in **3 ms**, a 120-second wait with Tasks returned `resultType=task` in **1 ms**, `tasks/get` reported working, `tasks/cancel` settled cancelled, and the durable Run remained `queued`.
- [x] Real raw-stdio legacy compatibility smoke passed: `initialize` negotiated `2024-11-05`, `tools/list` still returned all 9 tools, and true stdin EOF shut down the SDK server naturally with exit code 0.
- [x] Real typed-output wire smoke passed after the final metadata build: `tools/list` returned `outputSchema` for **9/9** tools, success/error unions were visible for list/cancel surfaces, and actual `list_runs/get_run` structuredContent matched the documented Run fields.
- [x] `runwatch-mcp` focused tests: **4 passed, 0 failed** (2026 support, long-wait dispatch/bounds, tool roster, outputSchema completeness).
- [x] Cross-project Pi regression remained **31/31** after the MCP SDK migration, confirming the native `pi-runs -> runwatchd` primary path is unaffected.

Remaining R8:

- [x] MCP modernization completed in R8b with `rmcp 3.1.4`, 2026-07-28 Discovery/Tasks, legacy negotiation, 9/9 typed outputSchema and real stdio wire acceptance.
- [x] Installed-task resident start/recovery acceptance completed in R8c, including child crash, supervisor crash, Task Scheduler reconciliation and Job Object cleanup.
- [ ] Multi-hour resident daemon/HPC soak remains release hardening rather than a state-machine correctness blocker.

### Current round validation — 2026-09-01

- `cargo fmt -- --check` — passed after the final R8c supervisor/Task Scheduler lifecycle changes.
- `cargo check --all-targets` — passed; only the existing `russh v0.54.5` future-incompatibility warning remains.
- `cargo test --all-targets` — **76 passed, 0 failed, 5 ignored by default** after the 2026-09-01 R2c SSH-policy closeout. The ignored gates are real Pi offline continuation, real Windows Task Scheduler XML registration, real two-layer resident fault acceptance, real gm00 Slurm batch acceptance, and real gyz-mn02 LSF batch acceptance; all non-Pi environment gates were explicitly run green in this round, and the real Pi gate already has prior acceptance evidence.
- New focused coverage includes Local Process PID+creation-time identity, started/armed/atomic-terminal protocol, local bounded logs, supervisor breakaway semantics, and explicit fail-closed diagnostics when Windows denies durable Job breakaway, in addition to the existing submission/probe/delivery/offline tests.
- `cargo build -p runwatch-cli` — passed; live daemon + pi-runs capability smoke confirmed `logs_backend=runwatch` and `cancel_backend=runwatch`.
- Isolated dual-process R1c CLI smoke — passed: daemon-backed `list`, `tick`, `wait missing` and `adopt_run_v1` all behaved through a unique `RUNWATCH_ENDPOINT`; `adopted-smoke` was visible to a separate CLI `list` process.
- Cross-project short-wait smoke — passed: pi-runs `auto` discovered `wait_run`, selected the runwatch backend and received canonical daemon error `unknown run missing-run`, proving the newly advertised capability is wired rather than a stale client stub.
- Real cap00 OpenSSH semantic smoke: `cargo run -p runwatch-cli -- hosts` — passed and resolved 50+ actual aliases/effective HostName/User/Port/ProxyJump values through `ssh -G`.
- Real cap00 lifecycle-transport smoke: freshly rebuilt `runwatch ping gm00` — **passed**, returning `runwatch-ok` and remote hostname `mn01`. This exercises effective `ssh -G`, actual UserKnownHostsFile verification, authentication and SSH exec result collection.
- Final SSH-parity direct/ProxyJump regression — **passed** with rebuilt code: `runwatch ping gm00 -> mn01` and `runwatch ping gyz-mn02 -> mn02`; the latter exercises effective jump-host resolution plus russh `direct-tcpip`. `IdentitiesOnly=yes` agent filtering and encrypted-key non-interactive policy are implemented and covered structurally; cap00's OpenSSH agent currently has no identities, so agent-auth real-key acceptance remains environment-gated.
- Real cap00→gm00 Slurm batch acceptance — **passed**: one russh batch command mapped two short live Slurm jobs using the generated sentinel/squeue/sacct protocol in 2.47s; test cleanup cancelled both and follow-up `squeue` was empty.
- Real cap00→gyz-mn02 LSF batch acceptance — **passed** on IBM LSF 10.1: one generated `bjobs` batch mapped two short live jobs in 7.17s, `bkill` cleanup ran before assertions, and follow-up `bjobs -J runwatch-lsf-batch-smoke` was empty.
- R8a/R8c real Windows Task Scheduler round-trip — **passed**: unique temporary task create/query-XML/delete verified TimeTrigger reconciliation + `supervise --interval 20`; CLI autostart status remained disabled afterwards.
- R8c real resident fault acceptance — **passed**: `serve 74640 -> 38208` under the same supervisor, then `supervisor 55056 -> 11544` via Task Scheduler reconciliation with old child `38208` replaced by ready child `85204`; total 62.45s.
- Independent 2026-09-01 R8c rerun — **passed** in 63.36s: `serve 81092 -> 27088`, then `supervisor 82892 -> 27064` with old child `27088` replaced by ready child `86284`; post-test disposable tasks/processes/temp were all zero.
- R8c final system cleanup check — **passed** after removing one pre-fix orphan from an earlier acceptance round: zero disposable scheduled tasks, zero R8c `runwatch.exe` processes, zero R8c temp directories.
- R8a GUI full link build — **passed** in an isolated target directory and cleaned after validation.
- R8b real MCP 2026-07-28 stdio smoke — **passed**: Discovery/Tasks, 9 tools, no-Tasks fail-closed, Task create/get/cancel, durable Run unaffected.
- R8b typed-output stdio smoke — **passed**: 9/9 `outputSchema`, structuredContent success path, success-or-error schema unions.
- R8b legacy MCP smoke — **passed**: 2024-11-05 initialize negotiation, 9 tools, true stdin EOF -> natural exit 0.
- `pi-runs` current cross-project regression — **35 passed, 0 failed, 1 real-Pi live gate skipped by default**.
- Observation-aware pi-runs regression — **37 passed, 0 failed, 1 real-Pi live gate skipped by default**; live isolated smoke preserved `Run=running` while surfacing `health=unreachable/source=transport` as `Runs 1 running · 1 probe issue`.
- Final read-only service status — `daemon=disabled task=runwatchd  gui_autostart=disabled`; no production task was installed by acceptance testing.

### Repository baseline — completed 2026-09-01

- [x] Expanded `.gitignore` to exclude runwatch runtime state (`.runwatch/`, SQLite DB/WAL/SHM, logs and PID files) in addition to Cargo target/IDE outputs.
- [x] Pre-commit secret preflight found no OpenSSH/RSA private-key blocks or common inline API key/password/secret assignments.
- [x] Established the first repository baseline as commit **`ecf191a` — `feat: establish durable runwatch runtime baseline`**, containing the validated R0–R8/R2 implementation and documentation (43 files, 15,434 inserted lines).
- [x] This restores historical traceability before R9 and removes the source/documentation drift risk exposed during R8c.

## R9 — Codex CLI adapter

### R9a — exact-thread binding + durable submission — completed 2026-09-01

Architecture and acceptance:

- [x] cap00 currently runs `codex-cli 0.150.1`; the installed CLI exposes `codex queue --thread <THREAD> --message <TEXT>` and `codex exec resume [SESSION_ID] [PROMPT] --json`.
- [x] Codex app-server supplies the exact current thread id in MCP tool-call `_meta.threadId`; **the model never provides or chooses its own continuation thread id**.
- [x] Codex rollout JSONL has `session_meta` as the first record with `payload.id`, `payload.session_id` and `payload.cwd`. R9 reads only this metadata record for normal binding. The real 0.150.1 rollout exposed that both identity fields are present, so the locator requires `id == session_id` when `session_id` exists, then requires that identity to equal MCP `threadId` and that `cwd` still exists.
- [x] A nonexistent UUID passed to the installed `codex queue` fails explicitly (`no rollout found for thread id`) rather than reporting success, so `queue` is suitable as a live-first delivery attempt once the exact thread is bound.
- [x] Added typed MCP `submit_science_run` with **no thread/session identity argument**. rmcp 3.1.4 moves request `_meta` into `RequestContext.meta` before `ServerHandler::call_tool`, so the handler reads `threadId` from `RequestContext.meta`, cross-checks the exact persisted Codex rollout, builds `ContinuationBinding { agent_kind="codex", session_id, session_file, project_root, workspace }`, then calls daemon `submit_run_v2`. Existing MCP `submit_run` remains the legacy/adopt surface.
- [x] Generalized submission-time continuation validation for remote Slurm/LSF and Windows Local Process from `pi`-only to the explicit supported set `{pi,codex}`; unknown agent kinds still fail closed. This does **not** enable Codex terminal dispatch yet, so R9a cannot accidentally execute an incomplete continuation path.
- [x] Real persisted Codex metadata gate passed against cap00 thread `01a05753-3b45-7001-9ddf-131f7dedd1b5`: the locator found the exact rollout, validated `id == session_id`, and resolved its existing cwd while reading only the first JSONL record.
- [x] Real isolated MCP -> daemon -> gm00 Slurm gate passed: `_meta.threadId=019c1234-5678-7000-8000-000000000008` plus synthetic first-record-only Codex metadata produced Run `r9a_codex_mcp_0901_02`, Slurm Job **31738**, canonical `agent=codex`, exact `session_id`, project root sourced from session metadata, and `gm00:/share/home/shark/tmp`; after the submitting MCP process exited, a separate MCP client observed the same Run as **succeeded**.
- [x] R9a validation: `cargo fmt -- --check` passed; `cargo check --all-targets` passed with only the existing `russh v0.54.5` future-incompatibility warning; `cargo test --all-targets` — **82 passed, 0 failed, 6 ignored by default**. The sixth ignored gate is the real Codex rollout locator and was explicitly run green. Temporary local daemon/SQLite/CODEX_HOME smoke state was removed after acceptance.

### R9b — generic AgentInvocation + Codex offline driver — completed 2026-09-01

- [x] Generalized offline reservation/session leases from hard-coded `agent_kind='pi'` to the explicit supported set `{pi,codex}`. Pi still requires durable session file + pi-runs adapter; Codex requires a persisted rollout file but no Pi adapter. Ownership checks, orphan reconciliation, lease cleanup and process completion now key on `(agent_kind, session_id)` rather than silently using Pi leases.
- [x] `AgentInvocationRecord.session_file` / `adapter_path` are now optional durable fields so one invocation model can represent both adapters without fabricating a Pi extension path for Codex. Existing Pi JSON remains backward compatible and all Pi reservation/ownership tests stay green.
- [x] Added an explicit offline AgentAdapter dispatcher: `pi` keeps the already-accepted RPC/session lifecycle; `codex` uses native `codex.exe exec resume --json <thread_id> <bounded completion>`. Windows refuses `.cmd/.bat` shims for unattended Codex launch and allows an explicit native `RUNWATCH_CODEX_EXECUTABLE` override.
- [x] Codex completion prompts carry deterministic marker `[runwatch continuation delivery_id=...]`, Run/Attempt/Job/workspace metadata, and the explicit instruction not to resubmit the completed Attempt. Generated argv contains no `--approve` or dangerous trust/sandbox bypass flag.
- [x] Codex stdout is parsed as bounded JSONL: individual lines above 256 KiB are drained and discarded, stderr retains only the last 64 KiB, and success requires **exact** `thread.started.thread_id`, `turn.started`, `turn.completed`, exit 0, and no `turn.failed` / `error` / malformed event. A mismatched or absent thread identity becomes `needs_rebind`, never a silently accepted replacement thread; ordinary execution failure becomes retryable Delivery failure.
- [x] Added Codex durable-store coverage proving an independent `agent_kind=codex` lease is reserved without a Pi adapter and orphaned ownership requeues the same Delivery. Focused Pi offline tests remain green.
- [x] Added a default-ignored native subprocess acceptance with an isolated SQLite store and native `pwsh.exe` Codex stub. It exercises real child spawn, resume argv, bounded JSONL parsing, exact-thread evidence, Delivery ack and non-re-reservation; explicit 2026-09-01 run passed **1/1**.
- [x] R9b regression: `cargo check --all-targets` passed with only the existing `russh v0.54.5` future-incompatibility warning; `cargo test --all-targets` — **88 passed, 0 failed, 7 ignored by default**. The seventh ignored gate is the native Codex driver stub and was explicitly run green.

### R9c — rollout concurrency + crash-window idempotency — completed 2026-09-01

- [x] Inspected a real cap00 Codex 0.150.1 rollout by **record type/field names only**. Persistent turn boundaries are `event_msg/task_started { turn_id, started_at }` and `event_msg/task_complete { turn_id, ... }`; user prompts persist as `response_item/message role=user` with `content[].type=input_text`. Matching `task_started`/`task_complete` use the same `turn_id`, giving runwatch a durable settlement signal independent of `codex exec --json` stdout.
- [x] Added a bounded rollout scanner for the exact Delivery marker `[runwatch continuation delivery_id=...]`. It streams the bound rollout, skips individual unrelated records above 1 MiB, counts malformed records, and never stores general conversation content. It classifies `Idle`, unrelated `ThreadBusy`, exact `DeliveryRunning`, exact `DeliveryCompleted`, or fail-closed `Ambiguous`.
- [x] Codex spawn preflight now refuses a second active turn. An unrelated active turn causes retry/wait; an existing exact Delivery turn causes retry/wait instead of reinjection; a duplicate/unscoped marker or a marker turn that disappears without `task_complete` becomes `needs_rebind`. A marker turn older than the 6-hour worker bound also becomes `needs_rebind` rather than being duplicated.
- [x] New continuation is allowed only when the rollout has no active turn/no marker, contains no stable malformed record, and has been quiet for at least 3 seconds. This closes the practical race with an initiating Codex CLI that is still writing an active turn without inventing a second process registry.
- [x] Crash-window recovery is now rollout-backed: if the exact Delivery marker already has a matching persistent `task_complete`, runwatch marks the Delivery **delivered without resolving or launching Codex again**. This covers the critical crash after Codex completed its turn but before SQLite ack.
- [x] Rollout-state tests cover `ThreadBusy -> DeliveryRunning -> DeliveryCompleted`, unrelated completed-turn idle, duplicate/unscoped-marker ambiguity, and oversized unrelated records without losing later completion evidence — **4/4 passed**.
- [x] Default-ignored recovery acceptance seeds a real isolated SQLite Delivery in a non-acked state, writes persisted marker + matching `task_complete`, and invokes production `run_codex_invocation()` with a resolver that intentionally errors if called. Explicit run passed **1/1** and backfilled `delivered` without launcher resolution or re-reservation.
- [x] R9c regression: `cargo fmt -- --check` passed; `cargo check --all-targets` passed with only the existing `russh v0.54.5` future-incompatibility warning; `cargo test --all-targets` — **92 passed, 0 failed, 8 ignored by default**. The eighth ignored gate is the rollout-completed crash recovery and was explicitly run green. Pi/SSH/MCP/Slurm/LSF regressions remain green.

`codex queue` remains a possible future resident-app-server optimization, not a correctness dependency. Queue command success alone is never accepted as Delivery success; the durable rollout evidence above is the authority for idempotency and active-turn exclusion.

### R9d — final real Codex provider continuation acceptance — completed 2026-09-01

- [x] Started a real persisted Codex 0.150.1 provider thread in the runwatch repository with read-only sandbox and no dangerous bypass flags. A temporary global MCP entry `runwatch_r9d` pointed only at an isolated runwatch data dir/named pipe so the initiating Codex process and the later daemon-spawned `codex exec resume` saw the same MCP adapter without touching the normal runwatch ledger.
- [x] The initiating provider turn created exact thread **`01a05b2b-4cba-7632-9be0-3f3cc18ca3ab`**, called `runwatch_r9d.submit_science_run` exactly once through the real MCP `_meta.threadId` path, and submitted `r9d_codex_provider_20260901_1208_01` to `gm00:/share/home/shark/tmp` as Slurm Job **31739**. Canonical Run binding was `agent=codex`, the exact thread id, and project root `E:\\inter\\Documents\\Repos\\runwatch` sourced from persisted Codex `session_meta` rather than model arguments.
- [x] The initiating Codex process then exited normally after `R9D_SUBMITTED ...`; no wait/poll/continue message was sent. The scientific Run independently reached `succeeded`, after which resident runwatchd reserved the terminal Delivery and launched the Codex AgentAdapter offline.
- [x] Canonical isolated SQLite evidence after automatic resume: Delivery `r9d_codex_provider_20260901_1208_01:a1:terminal` was **`delivered` with attempts=1**, and its `AgentInvocation` was **`completed`** (PID 34736) with the same `agent_kind=codex` and exact session id; no retry or `needs_rebind` occurred.
- [x] Persistent rollout evidence independently confirmed exact-thread and exactly-once semantics: `session_meta.id == session_meta.session_id == 01a05b2b-...`, the deterministic continuation marker occurred **exactly once**, marker turn `01a05b2c-312e-7dc0-91f3-46c7478878b4` had a matching `task_complete`, and no active turn remained after the scan.
- [x] Acceptance cleanup removed the temporary `runwatch_r9d` global MCP entry, stopped the isolated daemon, and deleted isolated SQLite/temp logs. The real Codex rollout remains persisted as durable thread evidence. No human typed “continue”.

### R10 — Codex reference-adapter productization experiment — frozen after R10b on 2026-09-02

R9 proved that the durable continuation model can support a second, structurally different coding agent. R10a/R10b then proved conservative Codex onboarding and readiness diagnostics. That experiment has served its architectural purpose. **No additional Codex productization is part of the current v1 release path.** Existing Codex code remains regression-covered reference evidence while `runwatch` + `pi-runs` are completed first.

#### R10a — Codex MCP onboarding — completed 2026-09-01

- [x] Added `runwatch agent codex status|install|remove`. The implementation invokes Codex's own `codex mcp get/add/remove` management surface and never edits `~/.codex/config.toml` directly.
- [x] The registration name is `runwatch`; the expected command is the `runwatch-mcp` executable installed beside the current `runwatch` binary. `install` verifies the sibling exists, registers an absolute path, then round-trips `codex mcp get runwatch` before reporting success.
- [x] Ownership is fail-closed: only `transport=stdio`, no extra args, and an exact normalized command-path match are considered owned. A missing entry is installable/removable idempotently; a same-name entry pointing elsewhere is reported as `conflict`, and both install and remove refuse to overwrite/delete it.
- [x] `status` is read-only and reports Codex availability/version, expected MCP binary availability/path, and registration state (`missing`, `installed`, or `conflict`). A missing Codex executable is diagnostic output rather than a configuration mutation.
- [x] Windows child management uses native `codex.exe` (or explicit `RUNWATCH_CODEX_EXECUTABLE`) with `CREATE_NO_WINDOW`; unattended onboarding does not depend on PowerShell/Scoop `.ps1` forwarding semantics.
- [x] Real isolated Codex 0.150.1 acceptance exposed a Windows path issue: `canonicalize()` yielded `\\?\E:\...\runwatch-mcp.exe`. R8 had already shown verbatim paths are fragile for external Windows consumers, so onboarding now strips `\\?\` / converts `\\?\UNC\...` to ordinary DOS/UNC form before writing Codex configuration. Dedicated regression covers both forms.
- [x] Focused CLI tests after the fix: **4 passed, 0 failed** — owned parsing, missing/conflict fail-closed behavior, malformed-success rejection, and Windows verbatim path normalization.
- [x] Real management round-trip used an isolated temporary `CODEX_HOME` and the installed Codex 0.150.1: missing -> install -> second install idempotent -> remove -> second remove idempotent all returned success, and the persisted command was ordinary `E:\inter\Documents\Repos\runwatch\target\debug\runwatch-mcp.exe` with no verbatim prefix.
- [x] The same isolated acceptance seeded a conflicting `runwatch` MCP pointing at `C:\Windows\System32\cmd.exe /d /c echo conflict`: `runwatch agent codex install` and `remove` both returned nonzero, `status` reported `registration=conflict`, and the foreign entry remained unchanged until explicit test cleanup. Temporary `CODEX_HOME` was deleted afterwards; the user's real Codex configuration was untouched.
- [x] R10a closeout regression: `cargo fmt -- --check` passed; `cargo check --all-targets` passed with only the existing `russh v0.54.5` future-incompatibility warning; `cargo test --all-targets` — **96 passed, 0 failed, 8 ignored by default**. All prior Pi/Codex continuation, scheduler, SSH, service and MCP gates remain green.

#### R10b — read-only Codex doctor/compatibility — completed 2026-09-01

- [x] Added `runwatch agent codex doctor`. It is a bounded read-only aggregator over native Codex launcher availability/version, sibling `runwatch-mcp`, persisted Codex sessions root, exact owned MCP registration/enabled state, and runwatchd IPC compatibility. It never calls `mcp add/remove`, never reads credentials, and never scans conversation content.
- [x] Windows launcher readiness is stricter than onboarding discovery: unattended continuation requires a native executable and explicitly rejects `.cmd/.bat/.ps1` shell shims. The same native-launcher rule used by the offline Codex AgentAdapter is therefore visible before a scientific Run is submitted.
- [x] A missing `~/.codex/sessions` directory is reported as `not_yet_created` but does not block first use; existing non-directory session roots do block readiness. MCP must be owned by this runwatch install **and enabled**.
- [x] Daemon hello now advertises additive capability `offline_codex_continuation`. Doctor distinguishes `daemon=unreachable` from an older/incompatible daemon that responds but lacks the Codex continuation capability, instead of collapsing both into one error.
- [x] Final `ready=true` requires all execution-critical conditions together: native Codex launcher, sibling MCP binary, owned+enabled MCP registration, and a reachable Codex-capable daemon. Every failed condition is printed as an actionable reason and the command exits nonzero; `status` remains the lighter registration-only view.
- [x] Real isolated three-state acceptance on Codex 0.150.1 used an E:-only temporary `CODEX_HOME`, E:-only runwatch data dir and a unique named pipe: missing MCP + daemon offline -> nonzero with both reasons; installed MCP + daemon offline -> nonzero with only daemon reason; installed MCP + current isolated daemon -> **exit 0 / `ready=true`**. A second doctor call remained ready without mutations; fixture install/remove succeeded and the guarded E: temporary directory was deleted.
- [x] R10b closeout regression: `cargo fmt -- --check` passed; `cargo check --all-targets` passed with only the existing `russh v0.54.5` future-incompatibility warning; `cargo test --all-targets` — **98 passed, 0 failed, 8 ignored by default**. All prior Pi/Codex continuation, scheduler, SSH, service and MCP gates remain green.

#### R10c — repeatable Codex real-provider release acceptance — deferred post-v1

- [ ] Do **not** advance this gate during the current release cycle. The tracked R9d/R10 harness and real-provider evidence remain useful regression/reference assets, but Codex is not a v1 release blocker.
- [ ] After `runwatch` + `pi-runs` v1 is complete, revisit agent-neutral extraction before adding new Codex features. The intended product boundary is a future independent Agent Integration project (working name `codex-runs`) analogous to `pi-runs`; **no such repository is created in the current phase**.
- [ ] Long-term, `runwatchd` should own only agent-neutral Delivery/AgentInvocation contracts. Codex-specific `_meta.threadId`, rollout parsing, `codex exec resume`, MCP registration and doctor behavior belong in that future integration plane. The embedded implementation is transitional reference code, not architecture to extend.

Safety invariants:

- Never pass `--dangerously-bypass-approvals-and-sandbox` or `--dangerously-bypass-hook-trust` for unattended continuation.
- A missing/ephemeral rollout, thread-id mismatch, unavailable project cwd, or ambiguous resume is fail-closed and becomes retry/needs-attention, never a silently-created replacement thread.
- Codex is an AgentAdapter only. Run state, scheduler ownership, observation and Delivery retry remain exclusively in `runwatchd`.
- A subprocess exit code is never sufficient Codex continuation evidence; exact thread identity and completed-turn evidence are mandatory.

## R11 — Pi-first v1 production closure — in progress 2026-09-02

The current development priority is to finish the already-proven Pi product path before any further AgentAdapter expansion.

### R11a — release/distribution foundation — completed 2026-09-02

- [x] Added workspace `xtask` as the formal Rust-native release tool. `cargo run -p xtask -- package` builds the release `runwatch`, `runwatch-mcp` and `runwatch-gui` sibling binaries and emits one deterministic ZIP; Python is not a packaging/runtime prerequisite. Existing untracked Python packaging experiments remain prototypes only and are not part of the release architecture.
- [x] Package integrity is self-describing and fail-closed: `release-manifest.json` records schema/version/platform, required sibling layout, payload sizes and SHA-256; `SHA256SUMS.txt` covers exactly payload + manifest; `cargo run -p xtask -- verify <zip>` rejects wrong roots, missing/unexpected files, duplicate sums and size/hash mismatches.
- [x] Release creation never overwrites an existing archive. It creates a same-directory `.partial-<pid>` file, fsyncs it and promotes it only after ZIP completion; an existing final archive is preserved and causes an explicit error instead of deletion/replacement.
- [x] The first real Windows release build exposed a 1 MiB stack-local hashing buffer that overflowed the xtask main thread after Cargo had successfully built all three release binaries. Hashing now uses a 64 KiB heap buffer and has a 4 MiB regression test; the repeated real package gate then passed.
- [x] Two packages generated independently from the same release binaries were byte-identical: `runwatch-v0.1.0-windows-x86_64.zip`, **9,683,276 bytes**, SHA-256 **`33d4b79377caad717ab32aadc1d3599c6b50b860e9ba8c7461309295d8ac7198`**. Both passed `xtask verify` with 5 manifest payload files.
- [x] The first package was expanded into an isolated ignored `dist/r11a-unpacked` directory and its packaged `runwatch.exe --help` executed successfully (`UNPACKED_SMOKE=PASS`), proving the CLI runs from the extracted sibling layout rather than `target/` or the source execution path. `docs/INSTALL.md` now documents the Pi-first install boundary and Rust-native package/verify commands.
- [x] R11a final regression: `cargo fmt -- --check` passed; `cargo check --all-targets` passed with only the existing `russh v0.54.5` future-incompatibility warning; final `cargo test --all-targets` passed **102 tests / 0 failed / 8 ignored**, including the new large-payload hashing regression.

### R11b — Pi installation/readiness closure — completed 2026-09-02

- [x] The supported boundary is now explicit across runwatch `docs/INSTALL.md` and pi-runs README/design: the runwatch portable package and Pi package are installed separately; `pi-runs -> local IPC -> runwatchd` is the only v1 durable authority path, pi-runs copies no runwatch binaries and manages no second daemon. Pi-facing `runs_doctor` is read-only and reports protocol/service/storage identity plus the complete required Pi v1 capability contract; legacy remains explicit migration-only.
- [x] Release-layout resident smoke passed from the R11a extracted package. Packaged `runwatch.exe supervise --interval 1` (supervisor PID **68628**) launched IPC-ready serve PID **50472** on isolated state/pipe; pi-runs doctor returned `ready=true` with zero missing capabilities and Pi 0.84.4 loaded the modified extension successfully.
- [x] Resident recovery from the package also passed: after deliberately terminating serve PID 50472, the same packaged supervisor replaced it with PID **39760**; `daemon-status` recovered and pi-runs doctor again returned `ready=true`. Test cleanup stopped the supervisor and remaining child processes; no normal runwatch store/configuration was used.
- [x] Fail-closed readiness is covered on the Pi side for daemon-unavailable, missing capability, unexpected service identity and explicit legacy selection. A real default-endpoint probe while no resident daemon was running returned `ready=false`/ENOENT rather than selecting the legacy ledger.

### R11c — repeatable real-Pi release acceptance — completed 2026-09-02

- [x] Turn the already-passed Pi real-provider + gm00 Slurm + exact-session continuation loop into a repeatable explicit release gate using isolated state and bounded cleanup.
- [x] Include the Windows Local × Process path as a second execution shape without duplicating continuation semantics.
- [x] The new Pi release harness is now exercising the packaged R11a runtime rather than `target/`. The Windows Local × Process shape passed with a real Pi/provider turn and isolated runwatch state: Run `r8b_local_20260902103203-a551306d` reached `succeeded`, its terminal Delivery reached `delivered` in one attempt, one offline AgentInvocation reached `completed`, and the exact persisted Pi session contained one completion + one settlement receipt before verifying the local marker through the normal Pi `read` tool.
- [x] The release harness keeps the continuation contract common across execution shapes: only Run submission/workspace verification differs; Delivery/Invocation/exact-session settlement checks are shared. Evidence is preserved for audit while spawned Pi/supervisor process trees are explicitly stopped in `finally`.
- [x] The first packaged Slurm release attempt exposed an undocumented workspace invariant rather than a scheduler failure. Job **31746** ran on `c02` and `sacct` reported `COMPLETED 0:0`, but the acceptance used `gm00:/tmp`: login-node `/tmp` is `/dev/sda3` while `$HOME=/share/home/shark` is on shared `192.168.100.20@o2ib:/share`. Because `sbatch` copies the script before execution, the job could succeed while its marker/sentinel/logs were written to `c02`'s node-local `/tmp` and were invisible from gm00. README/install and the Pi-facing contract now explicitly require a login+compute shared persistent workspace; node-local `/tmp`/scratch is unsupported unless the cluster guarantees sharing.
- [x] The corrected shared-workspace Slurm release gate passed on `gm00:/share/home/shark/tmp`: Slurm Job **31750** reached `succeeded`; terminal Delivery reached `delivered` in one attempt; exactly one offline AgentInvocation reached `completed`; the exact Pi session persisted one completion + one settlement receipt and verified the remote token through `runs_status`, `runs_logs`, explicit `ssh_activate`, and `ssh_read` before the exact release-success marker. No resubmission occurred.
- [x] The acceptance harness itself is now repeatable enough for release use: Pi tool-generated neutral defaults are normalized only through an explicit allowlist, evidence is preserved under unique ignored directories, spawned supervisor/Pi trees are stopped in `finally`, and the child-exit timeout is unreferenced/cleared so successful runs do not idle for the remaining 180-second timeout. Current pi-runs regression after these fixes is **45 passed / 0 failed / 1 skipped**.

### R11d — endurance/fault hardening — in progress 2026-09-02

- [x] pi-runs now carries the shared resident soak driver instead of duplicating scheduler/runtime logic in runwatch. It reuses one packaged `runwatch supervise` + SQLite + local IPC authority across real-provider rounds and injects serve-child failure only after initiating Pi sessions have successfully armed/settled their handoff and every Run has a persisted execution handle.
- [x] First corrected mixed qualification passed in **173.802 s**: Local Process `local:28288:01dd3ad1afc8c959` and gm00 Slurm Job **31752** were both `running` when isolated serve PID **57824** was killed; supervisor PID 69992 restored serve PID **46120**. Both Runs survived to `succeeded`; both terminal Deliveries were `delivered` in one attempt; both had exactly one completed offline AgentInvocation, one persisted Pi completion and one settlement receipt, with normal local/SSH result inspection.
- [x] An earlier qualification attempt caught the fault injector firing at `status=submitting` before Slurm IPC reply. That ambiguous Run was recovered through the existing same-run-id receipt-aware submission contract as Job **31751** and explicitly cancel-requested; the soak driver now requires durable `job_id` + non-`submitting` state before restart. This is harness hardening, not a hidden scheduler failure.
- [x] Completion/settlement crash-window qualification is now repeatable. A Local Process case killed isolated serve only after exactly one `runwatch/completion` existed and before any settlement receipt; supervisor recovery requeued the interrupted Invocation, emitted a recovery message instead of duplicating completion, and finished with Delivery attempts=2, AgentInvocation count=2, completion=1, settlement=1 and the exact result-verification marker. Evidence: `pi-runs/acceptance-output/soak-20260902134804-6418b747`.
- [x] Same-session branch divergence/rebind is now repeatable through Pi's exported `SessionManager` rather than JSONL mutation. The wrong sibling branch received zero completion/settlement, Delivery became `needs_rebind`, real Pi `runs_rebind` captured a current leaf descended from the generated branch marker, the same Delivery completed on attempt 2, and the live resumed Pi session persisted exactly one completion plus one settlement before the exact local verification marker. Evidence: `pi-runs/acceptance-output/soak-20260902142547-dab304ca`.
- [x] Transient SSH-loss/recovery is now repeatable without changing user SSH config, firewall or the remote sshd. `RUNWATCH_SSH_CONFIG` lets only the isolated runwatch supervisor use an evidence-local OpenSSH config routed through a localhost TCP relay whose host keys are derived exclusively from already-trusted `known_hosts`. gm00 Slurm Job **31753** stayed `running` with the same JobID while Observation moved `fresh -> unreachable(source=transport, Channel send error) -> fresh(source=scheduler)`, then succeeded with Delivery/Invocation/completion/settlement all exactly once. Evidence: `pi-runs/acceptance-output/soak-20260902142113-92dcbbd7`.
- [x] The OpenSSH config override is a normal transport boundary, not an acceptance-only bypass: alias discovery and `ssh -G` both consume the same explicit `RUNWATCH_SSH_CONFIG`, while unset behavior remains the user's normal `%USERPROFILE%/.ssh/config`/`$HOME/.ssh/config`. Full Rust regression after the override is **105 passed / 0 failed / 8 ignored**.
- [x] Rebuilt the actual current HEAD `4888d39` as a fresh Rust-native Windows release package in an isolated `%TEMP%` output root. `xtask verify` passed; the ZIP is **9,686,896 bytes**, SHA-256 `7bcfe151b56400de2695ff6e8f56f13743691e77c48cda282098b95295edd8f1`.
- [x] The extracted current package then passed a **456.473 s / 2-round / 4-case** real Pi repeat qualification: concurrent Local + gm00 Slurm each round, serve restart each round, real same-session rebind each round, Slurm Jobs **31755/31756**, exactly-once Slurm Delivery/Invocation/completion/settlement, and successful remote `ssh_activate/ssh_read` verification. This is stronger repetition evidence but remains below the formal multi-hour threshold.
- [x] A focused **Slurm-only packaged rebind** follow-up passed after pi-runs made remote verification extensions explicit under `--no-extensions`. Against the same verified ZIP SHA-256 `7bcfe151b56400de2695ff6e8f56f13743691e77c48cda282098b95295edd8f1`, gm00 Job **31757** completed after real same-session divergence: wrong-branch completion/settlement stayed zero, one explicit `runs_rebind` reset the same Delivery, final Delivery succeeded on attempt 2, and Pi completed `runs_status/runs_logs/ssh_activate/ssh_read`. Evidence: `pi-runs/acceptance-output/soak-20260902151210-47242b5c`.
- [x] pi-runs now supports fail-closed **resumable endurance evidence** for the final multi-hour gate. One endurance session freezes the runwatch executable hash, active pi-runs runtime/acceptance tree, Pi API package code, pi-ssh-tools code, model/workspace/target/fault cadence, and reuses one SQLite data directory + IPC identity across immutable `segment-NNNN` checkpoints with monotonic rounds. Only clean successful active time accumulates; failed/interrupted/incomplete/ambiguous/missing segments make that evidence session non-resumable and release-blocking.
- [x] Real resume qualification passed against this packaged runwatch: Local Process segment 1 ran **109.485 s** (round 1), segment 2 resumed the same nonce/SQLite/IPC for **83.710 s** (round 2), cumulative clean active time **193.195 s**, with `target_met=false` against a deliberate 600 s qualification target. A changed-cadence resume was rejected before creating segment 3. Evidence: `pi-runs/acceptance-output/soak-20260902152524-92b2d188`.
- [x] The first formal **7200 s** evidence session (`pi-runs/acceptance-output/soak-20260902153301-b21f6ed7`) was correctly preserved as a failed/non-resumable release blocker. Round 1 passed concurrent Local + gm00 Slurm Job **31758** across serve restart **44564 -> 68800** with exactly-once continuation. During round 2, one real Pi seed provider suffered **524 -> 524 -> 503** and recovered only roughly seven minutes later; the earlier Slurm Job **31759** had already completed its old 60-second workload before the scheduled SSH-cut boundary. The driver therefore failed closed with `scheduled SSH fault requires an active Slurm Run`; `failure.json` records **624.521 s / 1 completed round** and the read-only evaluator reports `dirty_segments=[failed:1]`, `qualified=false`, zero credited endurance time. This is a harness/workload-timing failure, not a hidden runwatch state-machine corruption, and that evidence session must never be resumed.
- [x] Formal endurance timing is now explicit instead of empirical: pi-runs has a separate bounded initiating-provider `seed_timeout_sec` (**60..540 s**); any >=7200-second session with fault injection requires `run_delay_sec >= seed_timeout_sec + 60 s`; and Slurm walltime is generated as workload delay + 120 seconds instead of fixed at 2 minutes. The next formal profile is delay **600**, seed timeout **480**, round timeout **1200**, preserving enough active-work window even under long provider retry.
- [x] New clean formal session #2 (`pi-runs/acceptance-output/soak-20260902155301-4ddcb294`) has a valid first segment under that profile. Segment 1 ran **1004.342 s**: Local `local:23420:01dd3af33ac37e80` and gm00 Slurm Job **31760** were both active before serve restart **50636 -> 84340**; both then reached `succeeded` with Delivery attempts=1, AgentInvocation=1, completion=1 and settlement=1. Local verified via `runs_status/runs_logs/read`, Slurm via `runs_status/runs_logs/ssh_activate/ssh_read`. Evaluator state is `segments=1`, clean active time **1004.342 s**, `dirty_segments=[]`, mixed Local+Slurm and one restart recovery; it remains correctly unqualified until duration and repeated SSH/rebind/settlement-crash coverage accumulate in the same frozen session.
- [x] Formal segment 2 passed in **706.141 s**, cumulative clean active time **1710.483 s**. Local `local:17888:01dd3af5f0ab3f7a` and gm00 Slurm Job **31761** were active across restart **73392 -> 47236**. The same Slurm Job stayed `running` through evidence-local SSH transport `fresh -> unreachable(Channel send error) -> fresh`; Local same-session divergence produced zero wrong-branch completion/settlement, then exactly one explicit rebind recovered the same Delivery on attempt 2. Evaluator now has 2 Local + 2 Slurm cases, 2 serve restarts, 1 SSH recovery, 1 rebind recovery, 0 settlement-crash recoveries, and no dirty segments.
- [x] Formal segment 3 / round 3 passed in **758.707 s**, cumulative clean active time **2469.190 s**. Local `local:77000:01dd3af7e64667b7` and gm00 Slurm Job **31762** were active across serve restart **6652 -> 86896**. The Local continuation reached the exact crash window with completion=1 and settlement=0; isolated serve was killed **86896 -> 80156**, then orphan recovery completed the same Delivery with attempts=2, AgentInvocations=2, final completion=1 and settlement=1. Slurm also survived the global crash with attempts=2 / invocations=2 but one final completion/settlement and normal `ssh_activate/ssh_read`. Evaluator coverage is now rounds=3, serve restarts=3, SSH recoveries=1, rebind recoveries=1, settlement-crash recoveries=1, `dirty_segments=[]`.
- [x] Formal segment 4 / round 4 passed in **761.042 s**, bringing cumulative clean active time to **3230.232 s** with zero dirty segments. Local `local:27328:01dd3afa2f9ade79` and gm00 Slurm Job **31763** were active across serve restart **73208 -> 60784**. The second scheduled SSH cut recovered `fresh -> unreachable(source=transport, Channel send error) -> fresh(source=scheduler)` on the same running Slurm JobID; the second real same-session Local divergence delivered zero completion/settlement to the wrong branch, then exactly one `runs_rebind` recovered the same Delivery on attempt 2. Coverage reached rounds=4, Local=4, Slurm=4, serve restarts=4, SSH recoveries=2, rebind recoveries=2, settlement-crash recoveries=1. At this checkpoint `qualified=false` was due only to active time <7200 s and settlement-crash recovery occurring once rather than twice.
- [x] The same formal authority is now intentionally **dirty/non-resumable** after segment 5 exposed an acceptance-verifier false negative rather than a durable runwatch/pi-runs state failure. Round 5 passed Local + gm00 Slurm Job **31765** exactly once across serve restart **41236 -> 89504**. In round 6, Job **31766** reached the intended completion=1/settlement=0 crash window and recovered to final completion=1/settlement=1 on Delivery attempt 2; the Local sibling branch still received zero completion/settlement before explicit rebind and then settled exactly once. The global serve crash also retried that Local Delivery, so its correct final attempt count was 3. The pre-`d279a58` verifier incorrectly hard-coded rebind attempt 2 and failed segment 5 after **1552.697 s**. The evidence remains `dirty_segments=[failed:5]` and no segment-5 time is credited or repaired in place.
- [x] pi-runs commit **`d279a58` (`test: fix combined endurance retry accounting`)** fixes that verifier without weakening exactly-once invariants. Its `faultAttemptBounds()` accounts for a case's own rebind/settlement-crash retry sources and permits at most one additional retry/Invocation caused by another case's injected global serve crash; a focused regression covers the observed rebind + global settlement-crash restart shape. pi-runs default regression is **53 passed / 0 failed / 1 skipped**.

- [x] Started formal endurance authority #3 at `pi-runs/acceptance-output/soak-20260903003939-cf822303` from corrected pi-runs `d279a58`, keeping packaged runwatch `rc-4888d39`, `四二/gpt-5.6-luna`, mixed Local Process + gm00 Slurm on `/share/home/shark/tmp`, delay 600 / seed timeout 480 / round timeout 1200 and the frozen restart/rebind/settlement-crash/SSH cadence. Round 1 is clean: Local `local:93672:01dd3b3cbc51d937` + Slurm Job **31770** remained active across serve restart **91264 -> 94108** and both succeeded; each has Delivery attempt=1, AgentInvocation=1, completion=1, settlement=1 with exact result inspection (`runs_status/runs_logs/read` locally, `runs_status/runs_logs/ssh_activate/ssh_read` remotely). The same endurance process continued into round 2 and already exercised a second serve restart **94108 -> 93504**.
- [x] Formal authority #3 is preserved as **failed/non-resumable** after segment 1 failed in round 2 at **1347.466 s**. The runwatch-owned durable path did complete: the Local Run succeeded, explicit `runs_rebind` returned `rebound=true`, the rebound exact Pi session received one completion, `runs_status/runs_logs/read` completed, and the marker file contained the exact expected token. The real provider then copied that already-verified token incorrectly in its terminal free-text `R8B_RELEASE_OK` acknowledgement by omitting `cf822303`, so pi-runs' existing exact-string verifier failed closed. No failed-segment time is credited and the authority must not be resumed or repaired.
- [ ] Keep runwatch durability semantics unchanged while pi-runs separates deterministic release evidence from nondeterministic free-text copying: exact marker tool-result verification, verification-tool contract and exactly-once Delivery/Invocation/completion/settlement remain hard gates; only the redundant byte-perfect assistant echo should cease being the authority for a token already verified by the tool result. Any pi-runs verifier edit requires a new frozen formal authority.
- [ ] Treat any endurance failure as a release blocker; do not hide it behind adapter-specific retry loops.

### R11e — compatibility retirement + release candidate contract — completed 2026-09-02

- [x] Decided the legacy data boundary: keep only the one-time read-only `runs.jsonl -> SQLite` importer when the canonical runs table is empty. Current `RunRecord`, JSON schema and MCP output no longer expose `on_complete/on_success/on_failure/acked_at`; a regression feeds those old fields into the importer and proves they are ignored while the latest historical Run still imports. Hidden CLI callback flags remain only to fail closed with an explicit migration message.
- [x] Added daemon build-version diagnostics to IPC `hello` while keeping compatibility capability-based: Pi v1 still requires protocol 1 + service/storage identity + its required capability set; an exact runwatch version match is intentionally **not** a compatibility gate.
- [x] Real cross-project isolated doctor smoke passed after rebuilding the actual `target/debug/runwatch.exe`: current pi-runs reported `ready=true`, `runwatch.version=0.1.0`, protocol 1 and zero missing capabilities against a unique named pipe/data store. This also documented that `cargo test` alone does not refresh the ordinary debug binary used by external smokes; cross-project gates must build the executable they launch.
- [x] Added `docs/V1_RELEASE_CANDIDATE.md` as the canonical v1 scope/compatibility/package/gate checklist. RC contract is frozen; tagging remains blocked on R11d endurance evidence, not on new AgentAdapter work.
- [x] Closed resident install/upgrade/uninstall stop semantics. `autostart --remove` now `/End`s the owned Task Scheduler task, refuses to delete registration while an unverified runtime owner remains, and when `/End` leaves the task-owned supervisor child alive it terminates only the supervisor PID proven by `supervise.lock` + `supervise.pid` (no `/T` process-tree kill), then waits for both supervisor/serve locks to release before `/Delete`. This preserves breakaway scientific Local Process jobs and fails closed around independent/manual owners.
- [x] Real disposable Task Scheduler acceptance proved the Windows edge: `/End` alone had `natural_release=false`; verified supervisor PID **87260** was then terminated, its Job Object removed the serve child, and `runtime_released=true`. The unique task was deleted and evidence was preserved at `C:\\Users\\inter\\AppData\\Local\\Temp\\runwatch-r8c-28472-1788353950682680400`. The earlier intentionally stricter gate failed first and exposed this exact `/End` child-survival behavior instead of hiding it behind cleanup.
- [x] Upgrade/uninstall docs now require old-package stop before binary replacement, whole sibling-directory replacement, reinstall/readiness verification, GUI closeout, and explicit preservation of `%USERPROFILE%\\.runwatch` state. Service stop is not Run cancellation; remote scheduler jobs and breakaway Local Process workloads remain durable across the maintenance gap.

## Known transitional debt

Until later phases complete, these are explicitly compatibility paths, not architecture to extend:

- legacy `runs.jsonl` may remain as one-time migration/export compatibility input, but is no longer canonical storage.
- MCP is now on `rmcp 3.1.4`; remaining MCP debt is release interoperability breadth (more third-party clients), not a handwritten protocol implementation.
- Host values use `ssh -G`; recursive `Include` alias discovery, effective Global/UserKnownHostsFile + HostKeyAlias, raw/user/port/IPv6 ProxyJump parsing, `IdentitiesOnly`-filtered OpenSSH-agent/Pageant fallback and encrypted-key non-interactive policy are implemented. `RUNWATCH_SSH_CONFIG` optionally selects one explicit OpenSSH config for both alias discovery and `ssh -G` without changing HOME; this enabled isolated transport-fault qualification while preserving Pi/provider configuration. Trust-relaxing OpenSSH options are intentionally not mirrored: runwatchd still requires pre-existing known-hosts trust.
- scheduler observation has first-class Observation rows, adaptive polling, Slurm v2 batching and LSF active/recent batching with `bhist` fallback. R2 execution/observation/SSH parity is closed. Pi and Codex both have real continuation evidence, but only the Pi path is in the current v1 release scope; remaining work is Pi-focused distribution/readiness, repeatable release acceptance, compatibility retirement and long-soak hardening.

## Release gate for the current v1 project

The first production release is intentionally **Pi-first**. The blocking product loop is:

```text
Pi on Windows
  -> prepare/inspect remote workspace with pi-ssh-tools
  -> runs_submit through pi-runs to runwatchd
  -> Pi exits completely
  -> remote Slurm/LSF or local Process computation runs independently
  -> runwatch survives reconnect/restart conditions
  -> terminal Delivery restores the exact Pi session/branch
  -> Pi explicitly re-activates the recorded workspace
  -> Pi inspects results and continues scientific reasoning
```

This functional loop already has real provider/HPC/crash evidence. V1 closes only after the same semantics are repeatable from the supported release/install layout and survive the R11 endurance matrix. The existing Codex loop remains valuable second-adapter evidence but is **not** a v1 release gate. No human should need to type “continue”.
