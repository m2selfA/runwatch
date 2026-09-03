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
- formal segment 2 passed and raised cumulative clean active time to 1710.483 s. It adds second serve restart, real SSH loss/recovery on Job 31761, and one real branch-rebind recovery with zero wrong-branch delivery; no dirty segments.
- formal segment 3 is clean: cumulative active time 2469.190 s. Local completion-before-settlement crash occurred at completion=1/settlement=0, serve 86896 -> 80156 recovery finished the same Delivery with attempts=2/invocations=2 but still exactly one completion + settlement; Job 31762 also recovered without duplicate terminal delivery. Settlement-crash coverage is now 1, with no dirty segments.
- formal segment 4 is clean: cumulative active time **3230.232 s**. Both Local + Job **31763** survived serve **73208 -> 60784**; the second SSH cut recovered `fresh -> unreachable -> fresh` on the same Slurm JobID, and the second Local same-session divergence recovered through exactly one `runs_rebind`. At that checkpoint repeated serve restart/SSH/rebind requirements were satisfied; only total clean duration and a second settlement-crash recovery remained.
- formal session #2 is preserved as non-resumable after segment 5 exposed an acceptance-verifier false negative. Round 5 passed Local + Job **31765** exactly once across serve restart **41236 -> 89504**. In round 6, Job **31766** reached completion=1/settlement=0 and recovered to final completion=1/settlement=1 on Delivery attempt 2; the Local sibling branch received zero completion/settlement before explicit rebind and then settled exactly once. Because the same global crash also retried the Local Delivery, the correct final attempt count was 3; the old verifier incorrectly required rebind attempt 2 and failed segment 5 after **1552.697 s**. The evidence remains `dirty_segments=[failed:5]` and contributes no additional formal time.
- pi-runs **`d279a58`** corrects combined-fault retry accounting: each case must account for its own rebind/settlement-crash retries and may receive at most one extra retry/Invocation from another case's injected global serve crash. The focused regression covers the observed combined shape; default pi-runs tests are **53 passed / 0 failed / 1 skipped**.

Additional formal evidence:

- authority #3 `soak-20260903003939-cf822303` is preserved as failed/non-resumable. Round 1 passed Local + gm00 Slurm Job **31770** across serve **91264 -> 94108**. In round 2 the Local runwatch/rebind/session path still reached one terminal completion, successful `runs_status/runs_logs/read`, and an exact marker-file token; the real Pi provider then omitted `cf822303` only while copying that verified token into its terminal free-text `R8B_RELEASE_OK` acknowledgement. The current pi-runs verifier treats the byte-exact assistant echo as a hard gate and therefore failed segment 1 after **1347.466 s**. No failed-segment time is credited and no runwatch DB/session evidence is modified to compensate.

- pi-runs **`bcc9eaf`** now hardens that boundary without weakening deterministic runwatch invariants: exact token verification from the tool result, verification-tool allowlist/count/order, and exactly-once Delivery/Invocation/completion/settlement remain mandatory; one terminal `stop` acknowledgement must still bind to the completed Run, while its redundant copied token is recorded rather than treated as a second token authority. Focused release tests are **6/6**; default pi-runs regression is **53 passed / 0 failed / 1 skipped** with the real Pi loader passing. A read-only replay of authority #3's real failed JSONL verifies its durable state and explicitly reports the imperfect copied acknowledgement without rewriting evidence.

Additional cleanup qualification:

- clean pi-runs authority #4 `soak-20260903011346-0e69c771` passed **2229.353 s / 3 rounds / 6 cases** with mixed Local+Slurm, restart=3, SSH recovery=1, rebind=1 and settlement-crash recovery=1, all with final exactly-once continuation. It remains preserved and is not resumed because the acceptance tree changes next.
- pi-runs then fixed its acceptance-only concurrent-inspector cleanup: all case inspectors still start concurrently, but all settle before a rejection propagates, preventing the failure-path timer/poll leak seen after authority #3. Focused soak tests pass **17/17**; default pi-runs regression is **55 passed / 0 failed / 1 skipped** with the real Pi loader passing.

Still blocking a v1 tag:

- final pi-runs authority #5 `soak-20260903022319-d0074e45` is active from cleanup-fixed `596d2f6`. Round 1 passed Local `local:90324:01dd3b4b3acaedbe` + gm00 Slurm Job **31781** across serve **93556 -> 55368**, with exactly-once Delivery/Invocation/completion/settlement and exact result inspection.
- authority #5 round 2 passed the scheduled real rebind + SSH recovery: Local had zero wrong-branch completion/settlement, one `runs_rebind`, attempt 2 final settlement; Job **31782** remained the same running JobID through `fresh -> unreachable(transport) -> fresh(scheduler)`. Serve recovered **55368 -> 49176**.
- authority #5 segment 1 closed cleanly at **2174.382 s / 3 rounds / 6 cases**. Round 3 hit Local completion=1/settlement=0 and recovered across serve **89912 -> 94808** to attempts=2 / Invocations=2 with final completion/settlement=1/1; Job **31784** likewise survived the global crash with one final settlement. Current read-only coverage is restart=3, SSH=1, rebind=1, settlement-crash=1 and no dirty segments.
- authority #5 round 4 passed the second scheduled SSH + rebind recovery: Local had zero wrong-branch completion/settlement, one `runs_rebind`, attempt 2 final settlement; Job **31787** stayed the same running JobID through `fresh -> unreachable(transport) -> fresh(scheduler)` and then settled exactly once. Repeated SSH and rebind requirements are now satisfied.
- authority #5 is now preserved failed/non-resumable. Segment 2 failed after **760.528 s** when a real Local Process seed, after recovering from a 503, added non-neutral scheduler-only `mem="1G"` and `time="00:10:00"`; the frozen verifier rejected `mem`, and read-only reporting returns `dirty_segments=[failed:2]`. Segment-2 time/round-4 coverage is not credited; supervisor/serve cleanup is confirmed stopped.
- pi-runs acceptance now compares model submit args through production `buildSubmitSpec()`: Local Process scheduler-only extras pass only when the emitted runwatch spec remains exactly `resources:{}`, while Slurm/LSF resource changes, unknown args, retired wakeup/webhook changes and durable identity/command/workspace differences still fail. Regression is **55 passed / 0 failed / 1 skipped**; authority #5's real failing seed passes read-only replay without evidence mutation. A new frozen authority is required from this corrected tree.

- formal pi-runs authority #6 `soak-20260903032005-11ed23b0` is clean through segment 1 from corrected `76cf02f`: **2249.112 s / 3 rounds / 6 cases**, `dirty_segments=[]`. Round 1 passed Local + Job **31789** across serve **97376 -> 98184**; round 2 passed the first real rebind + SSH recovery on Local `local:8976:...` and Job **31790** across serve **98184 -> 97648**; round 3 passed the first completion-before-settlement recovery on Local `local:89760:...` across serve **97308 -> 93864**, with Job **31791** also recovering exactly once. Current coverage is restart=3, SSH=1, rebind=1, settlement-crash=1.
- authority #6 round 4 passed the second scheduled rebind + SSH recovery before the later failure: Local `local:97864:01dd3b5f5e9463c2` recovered through one explicit rebind with one final settlement; Job **31792** passed the paired SSH-loss/recovery verifier; serve recovered **94912 -> 71384**.
- authority #6 is now preserved failed/non-resumable. In round 5 the real Local provider changed the frozen `acceptance-output\\soak-...` workspace into a different `acceptance-output\\soak\\...` hierarchy in both workdir and command. That material semantic change correctly failed; product submit also rejected the nonexistent Local workspace. Slurm sibling **31793** was explicitly cancelled. Segment 2 ended after **702.391 s / one completed round** and read-only evidence now has `dirty_segments=[failed:2]`, with only **2249.112 s** credited.
- pi-runs acceptance now shortens formal Local transcription to stable `acceptance-output\\local-workspace` plus a relative unique marker while retaining strict production-normalized workdir/command equality. Focused tests pass **6/6**, default regression remains **55 passed / 0 failed / 1 skipped**, and real-provider Local smoke `soak-20260903050529-fbeb6359` passed **143.891 s / exit 0** using the short workspace in the actual Pi tool call. A new frozen formal authority may now start; all dirty authorities remain immutable and uncredited.

- formal pi-runs authority #7 `soak-20260903050918-ff7f558d` is active from `ab13e7d`, contract-tree SHA-256 `3d1f46dfe8c68a891ee48852b51f42ebf594f6cb70e2f864a52c46aa45113227`. Round 1 passed Local `local:54464:01dd3b626abc6639` + Job **31794** through the full 600-second workload and serve **95776 -> 100336**, then completed exactly-once result inspection/settlement and advanced to round 2. Continue only this authority.

- authority #7 round 2 passed Local `local:97124:01dd3b640a740ee4` + Job **31795** through the first real rebind + SSH-loss/recovery pair and serve **100336 -> 90924**, then advanced to round 3. The active authority now has SSH=1/rebind=1; round 3 is exercising the first settlement-crash recovery.

- authority #7 segment 1 is clean at **2176.025 s / 3 rounds / 6 cases**. Round 3 passed the first completion-before-settlement recovery: Local `local:96360:01dd3b65ae9c7d89`, Job **31796**, scheduled serve **90924 -> 96384**, crash-window serve **96384 -> 95748**, attempts/invocations=2 with one final completion/settlement. Current coverage is restart=3, SSH=1, rebind=1, settlement-crash=1, dirty=0. Resume only this authority.
- authority #7 segment 2 is also clean: **2277.415 s**, cumulative **4453.440 s / 6 rounds / 12 cases**, no dirty segments. Job **31797** supplied the second SSH+rebind pair, Job **31798** the normal round, and round 6 combined Local rebind with Job **31799** completion-before-settlement recovery. Coverage is restart=6, SSH=3, rebind=3, settlement-crash=2; all repeated-fault requirements are now true and exactly-once invariants remain intact.
- round 7 passed Local `local:102224:01dd3b718cd7b3da` + Job **31800** across serve **101076 -> 100428**. A Slurm seed 503 and two later provider stall-watchdog retries were absorbed inside the same Pi lifecycle; exact remote token inspection completed and both final Deliveries/Invocations remained exactly once before round 8 started.
- round 8 passed Local `local:100024:01dd3b7492d8d0c6` + Job **31801** across serve **100428 -> 96588**. The Slurm JobID stayed constant through `fresh -> unreachable(transport, Channel send error) -> fresh(scheduler)`; Local wrong branch got 0/0 completion/settlement before one explicit `runs_rebind` recovered the same Delivery on attempt 2. Both final sessions are exactly once and round 9 started automatically.
- formal authority #7 is now **qualified**: **7512.123 s clean active / 3 segments / 9 rounds / 18 cases**, zero failed segments. Round 9 completed another real completion-before-settlement recovery on Local `local:97096:01dd3b7680940149` + Job **31802** across serve **100024 -> 87468**; both recovered with attempts=2 / Invocations=2 and one final completion/settlement. Coverage is restart=9, SSH=4, rebind=4, settlement-crash=3; every machine requirement is true and `reasons=[]`.
- final RC replay then found one release-runtime blocker not covered by the long-delay endurance shape: short Job **31804** reached `succeeded` while initiating Pi still held the session lease, a live claim moved the terminal Delivery to `delivering`, and Pi exited before ack. After claim/lease expiry the old store left that Delivery stranded with zero AgentInvocations. RunStore now transactionally requeues expired `delivering` claims before either live or offline reservation; focused and full regressions pass. This runtime change means authority #7 remains immutable historical qualification but cannot be used as the sole endurance proof for the new release binary. A fresh formal authority from the repackaged fixed runtime is required.
- fixed runtime `f6b75b8` is now packaged and verified as ZIP SHA-256 `3b67fae3055bed5765ae587fd9b27d228ea84fb47f1a0c4c377a0317591e426f`. A real fixed-package 2-second Slurm replay passed on gm00 Job **31805** with `localCPA/gpt-5.6-luna`: exactly one doctor/submit, one delivered Delivery, one completed AgentInvocation, one completion/settlement and exact `runs_status/runs_logs/ssh_activate/ssh_read`. The real short-path gate is green; current-binary multi-hour qualification is the remaining evidence requirement.
- fixed-package Local also hit the repaired handoff path naturally: attempt 1 live ownership expired and attempt 2 completed through the only offline AgentInvocation, while persisted completion/settlement/result inspection stayed exactly once. The pi-runs clean-gate verifier now recognizes only this bounded retry shape in addition to direct attempt 1; wider duplicate/retry shapes remain rejected. Full pi-runs regression is **56/0/1**, explicit loader and live bridge are green.
- committed-tree fixed-package Local replay is also green. Fresh authority #8 is frozen at `soak-20260903084318-fa9d26c5` from pi-runs `4d27e15` + packaged runwatch `f6b75b8`, using the unchanged 7200-second mixed Local+Slurm fault contract. Current-binary qualification now depends on this authority reaching machine `qualified=true` with zero dirty segments.
- authority #8 round 1 passed Local `local:39288:01dd3b804bc8d542` + gm00 Job **31806** across serve **100908 -> 97884**, with one Delivery/Invocation/completion/settlement per case and exact result inspection. Round 2 is active on Local `local:100964:01dd3b81dac5f97d` + Job **31807** for the first SSH + rebind recovery.
- authority #8 round 2 also passed: serve **97884 -> 9680**; Job **31807** stayed on the same JobID through the scheduled `fresh -> unreachable(transport) -> fresh(scheduler)` SSH cut, while the Local sibling branch stayed completion/settlement=0/0 until one explicit rebind recovered the same Delivery on attempt 2. Both final sessions settled exactly once.
- authority #8 is preserved failed/non-resumable after round 3. Job **31808** and the Local peer completed the product path, and the Slurm session performed exact `runs_status/runs_logs/ssh_activate/ssh_read`, read the correct token and persisted `runwatch/completion-settled`; the provider then mistyped only the synthetic final acceptance acknowledgement. The revised pi-runs acceptance tree removes long-string transcription from that synthetic step: final ack is exact fixed `R8B_RELEASE_OK`, marker files are deterministic `r8b-<10hex>.txt`, and path/token/tool verification remains strict. Focused tests are 7/7, full pi-runs regression is 56/0/1, and real fixed-package Local `r8b_local_20260903093219-cedc027f` plus gm00 Slurm Job **31810** both pass the revised gate. A fresh authority #9 is required; #8 must never be resumed.
- authority #9 is now frozen from committed pi-runs `9048141` at `soak-20260903093547-cade1766` against unchanged packaged runwatch `f6b75b8`. The formal contract remains 7200 s mixed Local+Slurm on gm00 `/share/home/shark/tmp`, delay 600 / seed timeout 480, restart=1, rebind=2, settlement-crash=3, SSH-fault=2 for 8 s. Frozen runwatch executable SHA-256 is `3d1be0ac320947adbd29e5335a7894930e9ba8eb05366ae307ba39ccc0ac16a1`; pi-runs contract-tree SHA-256 is `e3564ca39dce09f4a00631eb5ef9bb17f1beb7b0a8a8e354447d91fdb28732c6`. Only #9 may accumulate current-binary endurance time.
- authority #9 segment 1 closed cleanly at **678.723 s / round 1 / 2 cases**, with zero failed segments. Local `local:18352:01dd3b87a02174c3` + gm00 Job **31811** stayed live across serve restart **101620 -> 5880**, both reached `Succeeded`, and both saved Pi sessions read their exact short marker/token, returned exact `R8B_RELEASE_OK`, and persisted one final settlement. Current coverage is Local=1, Slurm=1, restart=1; duration and repeated fault dimensions remain intentionally incomplete.

No human `continue` message is permitted in the formal continuation gates.
