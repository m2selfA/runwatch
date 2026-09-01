# 设计：runwatch

对应痛点见 [pain-points.md](pain-points.md)，阶段计划与实现状态见 [DEVELOPMENT_CHECKPOINT.md](DEVELOPMENT_CHECKPOINT.md)。

## 一句话

**runwatch 是 coding agent 长科学计算的 Durable Run Lifecycle Authority。**

Run 是产品对象；SSH 连接、Slurm / LSF JobID、sentinel 文件和 agent 进程都只是句柄。runwatch 的职责不是替代调度器，也不是成为远端 IDE，而是保证 coding agent 退出后，计算仍被可靠观察，并在终态时把 continuation 可靠地交回正确的 agent 会话。

目标场景首先是 Windows 桌面上的 Pi coding agent + 远端 Linux HPC，随后复用同一核心接 Codex CLI 等其它 agent。

## 三个平面与唯一权威

```text
Pi coding agent
│
├── pi-ssh-tools ── Remote Workspace Plane
│                  read / write / edit / shell / science inspection
│
├── pi-runs ─────── Agent Integration Plane
│                  Pi tools / session binding / continuation UX
│                       │
│                       │ local IPC
│                       ▼
└────────────────── runwatchd ── Durable Run Plane
                         │
                         ├── SQLite / Run / Attempt / Event / Delivery
                         ├── scheduler adapters
                         ├── narrow SSH transport
                         └── durable continuation dispatcher
```

权威边界固定如下：

| 组件 | Authority | 明确不负责 |
|---|---|---|
| **runwatch** | Run 生命周期、scheduler handle、远端观察、终态、delivery、重试 | 通用远端编辑器、科研推理 |
| **pi-runs** | Pi tool 语义、Pi session/branch binding、Pi continuation | 第二份 durable ledger、长期 scheduler polling |
| **pi-ssh-tools** | Pi 在线时的远端 workspace 读写/编辑/命令 | 长期 Run 生命周期、Pi 离线后的 watcher |

三个项目只共享协议语义，不共享进程内 SSH 对象。`pi-ssh-tools` 可以继续使用系统 OpenSSH；runwatch 为长期低频轮询保留独立持久 transport。

## 目标分层

```text
runwatch-gui ─────┐
runwatch-cli ─────┼── local IPC / named pipe ── runwatchd
runwatch-mcp ─────┘                              │
                                                ├── runwatch-core
                                                │   SQLite + state + events + delivery outbox
                                                ├── runwatch-ssh
                                                │   host-alias transport only
                                                └── runner adapters
                                                    Slurm / LSF / Local Process
```

**只有 `runwatchd` 可以修改 canonical Run state。** GUI、CLI、MCP 和 agent adapters 最终都必须退化成客户端，避免多 writer、多 SSH pool 和多套状态推进逻辑。

当前实现已经收敛到 single-authority：只有 `runwatchd` 打开 canonical `RunStore`、持有 SSH pool 并推进 scheduler/delivery；CLI、GUI、MCP 与 pi-runs 都是客户端。旧 `runs.jsonl` 只保留一次性 migration/import 兼容，历史 callback 字段只用于反序列化，运行时 shell callback 已移除。

## 核心对象模型

最终数据模型拆成独立对象，而不是把所有语义压进一条 `RunRecord`：

### Run

稳定的科研逻辑对象。例如“E82 defocus sweep”。一次 retry / resubmit 不创建新的逻辑 Run。

### Attempt

一次真实 execution：

```text
Run E82
├── attempt 1  Slurm 1288  NODE_FAIL
├── attempt 2  Slurm 1290  PREEMPTED
└── attempt 3  Slurm 1294  COMPLETED
```

Attempt 保存 transport、runner、host alias、job handle、exit code、raw scheduler state、开始/结束时间等。

### Event

不可变时间线：submitted、running、scheduler transition、SSH unreachable、terminal、delivery retry、continuation accepted 等。GUI、诊断和故障恢复以 Event 为证据，不覆盖历史。

### Observation

执行状态和“我们是否看得见它”是两个维度。当前实现为每个 `(Run, Attempt)` 保存一条最新 `ObservationRecord`：

```text
execution_status: submitting | queued | running | succeeded | failed | cancelled
source: local_process | sentinel | scheduler | compatibility | transport
health: fresh | probe_error | unreachable
observed_at
raw_state
reason
command_exit_code
```

`observed_at` 每次 probe 都刷新；只有 source/health/execution/raw/reason/exit-code 的语义变化才追加不可变 `observation_changed` Event。`stale` 是客户端根据 `observed_at` 年龄推导出的视图状态，而不是覆盖数据库里的最后 probe 结果。短暂 SSH 故障、scheduler 未识别新状态或远端命令异常只能改变 Observation health/reason，不能把一个已知 Running 的任务改写成 `unknown/lost`。`get_run`/`list_runs` 以 sidecar 方式暴露 observation，因此旧客户端的 Run wire shape 不需要改变。

### Artifact

runwatch 只做 inventory / metadata：路径、是否 required、大小、mtime、可选 hash。真正读取科研输出、再跑分析由 agent + `pi-ssh-tools` 完成。

### ContinuationBinding

说明 Run 完成后应该交回谁：

```text
agent_kind
session_id
session_file
origin_leaf_id
project_root
workspace_ref
binding_version
continuation_policy
```

Pi 的 session 是树；只记录 `session_id` 不足以防止 `/tree`、`/fork`、`/resume` 后把完成事件送到错误分支。

### Delivery

终态 continuation 是 durable outbox，不是一个 `spawn()`：

```text
pending -> delivering -> delivered
                    └-> retrying
                    └-> needs_rebind -> pending (explicit rebind)
```

保存 attempt、next_retry_at、last_error、幂等 delivery id、attempt count 与 accepted/delivered 状态。live Pi 通过 session lease 保护的 claim/ack 协议拉取 Delivery；只有 agent adapter 明确接受后才算 delivery 完成。branch 不匹配则进入 `needs_rebind`，由显式 rebind 把同一 Delivery 重新置为 pending。

## RemoteWorkspaceRef

三项目的最小共享远端工作区协议：

```text
RemoteWorkspaceRef {
    host_alias,
    cwd
}
```

它可以直接映射为 `pi-ssh-tools` 的 `host:/remote/path`，但 runwatch 不 import、调用或持有 `pi-ssh-tools`。恢复 Pi 后，由 pi-runs 指导模型显式 `ssh_activate` 后再检查科研输出；SSH mode 不应被悄悄持久恢复。

## Transport × Runner

“计算在哪里”和“由谁调度”必须正交：

```text
Transport       Runner
---------       ------
Local       ×   Process
SSH(alias)  ×   Slurm
SSH(alias)  ×   LSF
```

因此标准提交路径是 `runs_submit -> runwatchd`，由 daemon 先持久化 Run/Attempt，再通过 transport 提交 scheduler。不要以 `ssh_bash "sbatch ..."` 作为正常路径，因为 `sbatch` 成功到 Run 登记之间存在崩溃窗口。

保留 `adopt existing job` 作为兼容逃生口，但不作为 Skill 默认流程。

## Local × Process（Windows v1）

本地长科学计算不再使用 PowerShell `Start-Job`。`Local × Process` 与 Slurm/LSF 使用同一 `Run / Attempt / Delivery` 生命周期，但 transport 是本机 Windows process：

- `submit_run_v2` 先事务提交 `Run=Submitting` 和 Attempt，再创建 wrapper；
- wrapper 先原子写 `started.json`，等待 runwatch 写入稳定 `(PID + creation time)` receipt 并创建 `armed` 后才执行科学命令；
- science process handle 编码为 `local:<pid>:<creation-time>`，daemon 重启后以 creation time 防止 PID reuse 误认；
- terminal 是原子 `terminal.json`，stdout/stderr 与 lifecycle artifacts 由同一 daemon IPC 暴露；
- cancel 先持久化请求，最终状态仍由后续 observation 收敛。

Durability 的硬边界是 Windows Job Object。wrapper 必须用 `CREATE_BREAKAWAY_FROM_JOB` 真正脱离 daemon/supervisor 的生命周期；若宿主 Job 不允许 breakaway，runwatch 明确 `ERROR_ACCESS_DENIED` 并拒绝启动，**不会**退化成“daemon 一死科学任务也死”的伪 durable 模式。推荐生产路径是 Task Scheduler → `runwatch supervise` → `serve`；supervisor 的 Job Object 带 `BREAKAWAY_OK`，而 science process 由 `serve` 显式 break away。2026-09-01 的真实门禁在本地科学任务运行中终止 `serve`，新 daemon 接管同一 SQLite 后仍观察到原 science process 完成并收敛 `succeeded`。

## SSH 边界

runwatch 的 SSH 只服务于 Run lifecycle：

- deploy 由 runwatch 生成的最小 wrapper；
- submit；
- poll scheduler；
- cancel；
- 读取结构化 sentinel；
- bounded tail 已知 stdout/stderr。

**不提供**通用 read/write/edit/shell IDE API；这些属于 `pi-ssh-tools`。

Host identity 只接受 OpenSSH config 中的精确 alias。alias discovery 会递归 `Include`（含 glob、相对路径和 `~`），用 canonical visited set 防循环，只收集非 wildcard/非 negated 的 `Host` token；这个 discovery parser **不决定连接参数**。每个 alias 随后都调用 `ssh -G <alias>`，由 OpenSSH 自己计算 effective HostName/User/Port/IdentityFile/IdentitiesOnly/ProxyJump/GlobalKnownHostsFile/UserKnownHostsFile/HostKeyAlias，再把结果交给 russh 持久 transport。host key 对 effective Global + User known-hosts 路径去重后逐个 fail-closed 校验，并使用 HostKeyAlias（若有）；Windows 的 `__PROGRAMDATA__` known-hosts 路径也会展开，缺失的默认文件被忽略而实际存在文件的错误/不匹配不会被绕过。**runwatchd 的 unattended trust policy 固定比可交互 OpenSSH 更严格：不继承 `StrictHostKeyChecking=no/accept-new` 等自动信任模式，只接受已有 known-hosts 记录。** ProxyJump transport 在内部结构化解析 `[user@]host[:port]`、bracketed IPv6 和 raw IPv6，alias jump 使用它自己的 effective config，raw jump 也单独 `ssh -G`，不再继承目标 IdentityFiles。transport 使用 per-host session lock，并让 `ssh.alive_interval` / `ssh.cmd_timeout_sec` 真正控制 keepalive 与命令超时；一个 Host 的慢命令不再锁住整个连接池。多个未加密 effective IdentityFile 会依次尝试，失败后使用受 3 秒连接/认证上限约束的系统 agent fallback：Windows OpenSSH named pipe → Pageant，Unix `SSH_AUTH_SOCK`。`IdentitiesOnly=no` 时可尝试 agent 提供的 identities；`IdentitiesOnly=yes` 时只允许与 configured IdentityFile 的 public identity 匹配的 agent key。encrypted private file 从不触发 daemon passphrase prompt，也不写入第二 credential store：必须预先加载到 agent/Pageant，并保留可匹配的 IdentityFile public identity/相邻 `.pub`；否则 fail closed 并返回可操作错误。SSH exec 在收到 `Eof` 后仍继续等待 `exit-status`/`Close`，避免成功命令丢失 exit code。

## Scheduler probe

sentinel 是 fast path，不是唯一真相：

1. 若结构化 sentinel 存在且有效，使用它。
2. sentinel 缺失或损坏，继续查询 scheduler；不能直接变成 Unknown。
3. Slurm live 状态优先 `squeue`，terminal/history 以 `sacct` 为准；显式处理 OOM、PREEMPTED、NODE_FAIL、TIMEOUT 等状态以及数组/step 记录。
4. LSF 先使用 `bjobs -a` 覆盖 live/recent finished 状态；若已经查不到，再以 `bhist -l` 的历史完成/退出事件兜底，避免超出 recent-job 清理窗口后无法确认终态。
5. probe 失败更新 observation health，而不是破坏最后一次可信 execution state。
6. Run-level `logs` 只允许 bounded tail 当前 Attempt 已知的 stdout/stderr（当前上限 500 行、每流 64 KiB）；`cancel_run` 只发 scheduler cancel request 并记录 `cancel_requested` Event，最终 Cancelled 仍由后续 scheduler observation 确认。
7. 常驻 daemon 使用 Observation 驱动的 adaptive polling：首次/Queued/Submitting 仍按 base interval；远端 Running 默认 2× base；`probe_error`/`unreachable` 逐级退避，最长 5 分钟；Local Process 保持 base interval。显式 `tick` 永远绕过 defer 强制刷新。由于 due 时间来自持久化 `observed_at`，daemon 重启不会制造所有 Run 同时重新探测的惊群。
8. 同一 Host 上至少两个可验证的 runwatch-v2 scheduler Attempt 会进入 batch probe。Slurm 用一个 SSH exec 检查各自受控 sentinel，并对完整 JobID 集合各执行一次 `squeue` / `sacct`；terminal → squeue → sacct。LSF 用同样的 sentinel fast path 加一次 group `bjobs -a`；`bjobs` 能识别的 active/recent Job 在 batch 内完成，缺失成员故意留给同一 tick 的原单 Run `bjobs → bhist` history fallback。batch transport/command 失败写整组 Observation health 且不做同 tick N 次重连；legacy adopted/singleton 路径保持原 probe，以兼容性优先。

wrapper 写 sentinel 必须采用临时文件 + 原子 rename，并携带 schema version、run/attempt、status、exit_code、finished_at。

## Durable storage

目标 canonical store 是：

```text
%USERPROFILE%\.runwatch\runwatch.db
SQLite WAL
```

核心表至少包含：

```text
runs
run_attempts
run_events
observations
deliveries
continuation_bindings
artifacts
agent_session_leases
```

旧 `%USERPROFILE%\.runwatch\runs.jsonl` 只作为迁移输入/可读导出，不再作为最终多 writer 账本。

## 单实例与本地 IPC

最终只有 `runwatchd` 持有数据库写权限和 SSH pools。单实例必须依赖 OS 级互斥/文件锁，而不是“45 秒内 heartbeat 新鲜”这种推断。

Heartbeat 只用于 health/UX：

```text
lock = authority
heartbeat = liveness hint
```

CLI/GUI/MCP/pi-runs 通过 named pipe/local IPC 请求 daemon。长 `wait` 不应阻塞整个控制平面；订阅/等待必须可取消、可并发。

本地 IPC 不是“只要路径在本机就默认可信”。Windows named pipe 必须拒绝 remote clients，并用当前用户 SID + `SYSTEM` 的显式 DACL 限制连接主体；Unix 数据目录必须为 `0700`，socket 必须为 `0600`。客户端并发连接在 Windows 上只对 `ERROR_PIPE_BUSY` 做短时有界重试，daemon 不存在或其他错误继续立即失败。

Pi live bridge 使用短 TTL `AgentSessionLease`：同一 `agent_kind + session_id` 同时只允许一个 owner instance。lease 是 session JSONL 单写者边界的一部分；heartbeat/lease 过期允许崩溃恢复，显式 shutdown 则主动 release。

## Continuation delivery

scheduler terminal 与 agent wakeup 必须分成两个事务阶段：

1. 数据库事务：Attempt -> terminal；写 Event；确定性插入 pending Delivery。
2. live Pi session bridge 在有效 lease 下 claim Delivery；若 origin leaf 不在当前 branch，则 ack 为 `needs_rebind`。
3. 可投递 completion 通过 Pi custom message/follow-up 进入原科研上下文；接受后 ack `delivered`，失败 ack `retry`，claim 自身还有过期回收兜底。
4. 用户/agent 通过 `runs_rebind` 显式把 blocked Run 绑定到当前 Pi branch 后，同一 Delivery 重新 pending。Rebind transaction 必须同时更新 canonical `ContinuationBinding` 和所有尚未 claim 的 Delivery payload binding 快照；若已有 Delivery=`delivering` 则拒绝 rebind，不能改写 in-flight identity。
5. Pi 离线时由 R5 headless AgentInvocation 接手；live 与 offline worker 共享 session execution lease，禁止并发写同一 session。RPC stdin 只在该 Invocation 仍拥有 `delivering` Delivery 时保持打开；`delivered` / `needs_rebind` / `retrying` 后 daemon 关闭 stdin，使 session_start 阶段的 trust/branch blocked worker 也能可靠完成 shutdown。
6. Daemon crash recovery 以 lease + durable Invocation/Delivery ownership 为准，不以 PID 猜测为准。新 daemon 先给幸存旧 worker 一个 reconnect grace；worker 能用同一 owner 刷新 lease 就继续完成原 Delivery。lease 过期后才 reconcile orphan：未 ack 的 `delivering` 重新进入 retrying，已 `delivered` 的 Invocation 仅收口 completed，不重投。每个 offline worker 在真正注入 completion 前还必须通过 `invocation_id + delivery_id + owner_instance_id` ownership verification；被回收的迟到 worker只能退出，不能重复注入。
7. Pi session 本身提供第二层幂等证据。成功 `agent_settled` 先 append 一个不进入 LLM context 的 `runwatch/completion-settled` custom entry，再向 daemon ack delivered。重启 worker先扫描当前 branch：settled receipt 或“原 completion 后已有持久化 final assistant stop”都直接补 ack，不再次写 completion；原 completion 已存在但 turn 未完成/失败时，只发送 `runwatch/completion-recovery` 继续已有上下文。这样 daemon/IPC 在 session settle 与 durable ack 之间崩溃，也不会生成第二条 completion。

这消除旧实现中“第一次 callback 失败后永不再试”以及“进程 spawn 成功就错误地当成 agent 已接受”的问题。

旧 `on_success` / `on_failure` shell callback 已退休：历史字段仅保留用于旧 JSON/DB 反序列化，不再具有执行语义。新 continuation 一律进入 durable Delivery/outbox；CLI 若显式使用旧 callback 参数会 fail closed。

## Pi 首发路径

Pi 是第一优先级 agent adapter：

```text
Pi online
  pi-runs session bridge
      -> acquire/refresh exact-session lease
      -> claim pending Delivery
      -> verify session file + origin leaf lineage
      -> Pi custom completion follow-up
      -> ack delivered / retry / needs_rebind

Pi offline
  runwatchd
      -> acquire Pi session execution lease
      -> headless Pi RPC worker
      -> exact session/branch continuation
```

提交时 pi-runs 自动记录当前 Pi session file / session id / origin leaf，不让模型手填；binding 与 submission intent 同事务持久化。若 completion 到来时同一 session JSONL 的 active branch 已经通过 `/tree` 等方式离开 origin leaf，live/offline bridge 将 Delivery 标记为 `needs_rebind`，不能强投。`runs_rebind` 用当前 Pi session/leaf 显式更新 binding，并由 runwatch 在同一 transaction 中刷新未 claim Delivery 的 binding 快照后重新 pending；真实 `/tree -> needs_rebind -> runs_rebind -> delivered` 门禁已通过。`/fork` 产生新的 session file/id，因此继续恢复原绑定的 exact session 不属于 branch 误投。

恢复后的标准科研流程是：

```text
runs_status / runs_logs
ssh_activate <workspace>
ssh_read / ssh_bash / ssh_edit
继续科研推理
必要时再 runs_submit 下一轮
```

`runs_wait` 只保留为短同步等待，不是分钟到天级任务的默认路径。

## Service lifecycle

Windows resident runtime 使用当前用户的 Task Scheduler，但 Task Scheduler **不直接充当 `serve` 的 crash watchdog**。真实故障注入证明：一个 action 成功启动后即使最终 exit 1，`RestartOnFailure` 也不会按我们需要的语义重新拉起 daemon。因此最终任务动作是同目录的 `runwatch.exe supervise --interval 20`：`InteractiveToken + LeastPrivilege` 保留当前用户的 `~/.ssh/config` / keys / agent 环境；每分钟 `TimeTrigger` 做粗粒度 reconcile，登录触发和安装时 `/Run` 提供即时启动，`MultipleInstancesPolicy=IgnoreNew` 抑制重复 supervisor。supervisor 本身不读写 `RunStore`、不持有 SSH/scheduler，只监护 `serve` child，以 1/2/5/10/30 秒有界退避重启；Windows child 放进 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` Job Object，因此 supervisor 被结束时不会遗留 daemon。若已有 fresh daemon，supervisor 等待而不竞争 ServeLock。后续如评估真正 Windows Service，也不能默认切到 LocalSystem 并失去用户 SSH identity。

GUI 是纯 daemon 客户端：不打开 `RunStore`、不持有 SSH pool、不调用 `serve_loop`。daemon 离线时只显示离线，不接管 scheduler ownership；Pause 通过 IPC 控制 daemon。

GUI 登录自启动与 daemon resident service 是两个独立设置：`Start GUI with Windows` 仅控制 UI，`Keep runwatchd running` 才控制 Task Scheduler/supervisor service。Task Scheduler XML 在已验证的 Windows 环境中必须写成 UTF-16LE + BOM；canonicalized Win32 verbatim paths 还必须在进入 scheduler/cmd 前规范化成普通 DOS/UNC 路径。发布门禁不仅做 create/query-XML/delete，还必须真实启动 supervisor、强杀 child 验证秒级恢复、强杀 supervisor 验证下一次 reconcile 恢复，并确认 Job Object 没有留下孤儿 child。

## MCP

MCP 是通用适配面，不是 Pi MVP 主链路。Pi 首发仍优先 `pi-runs -> local IPC -> runwatchd`。MCP server 已迁移到官方 Rust SDK `rmcp 3.1.4`：通过 `server/discover` 支持 2026-07-28，同时保留 SDK 自身的 legacy initialize negotiation；当前 **10 个工具**都有 typed `outputSchema` 和 structuredContent，其中 `submit_science_run` 是 R9 新增的 Codex exact-thread durable submit surface。`wait_run <= 60s` 是普通同步工具，更长等待只有客户端声明 `io.modelcontextprotocol/tasks` 时才 materialize 为 MCP Task，且 Task cancel 只取消 wait handle，不取消科研 Run。这样 MCP Tasks 是 transport-level observation lifecycle，不是第二份 Run runtime。

### Codex CLI adapter（R9）

Codex 不复制 `pi-runs`。它优先复用 MCP 作为 agent binding surface：Codex app-server 在 tool call 的 `_meta.threadId` 注入真实 thread id。rmcp 3.1.4 negotiated dispatch 会先把 request `_meta` 从 `CallToolRequestParams` 取出并放进 `RequestContext.meta`，因此 runwatch-mcp 在进入通用 tool router 前从 **`RequestContext.meta`** 提取 `threadId`。模型参数里不出现 `thread_id`，从而消除“模型把 Run 绑定到错误会话”的自由度。

R9 的 binding 来源是两份互相校验的本地权威元数据：

```text
MCP _meta.threadId  ---- exact identity ----┐
                                            ├─> ContinuationBinding(agent_kind=codex)
~/.codex/sessions/**/rollout-*.jsonl        │
first record: session_meta { id, session_id, cwd } ┘
```

session locator 只读取 rollout 第一条 `session_meta`，要求 `payload.id == _meta.threadId`，且真实 0.150.1 中若同时存在 `payload.session_id` 则进一步要求 `session_id == id`；取 `payload.cwd` 作为 project root，并要求该目录仍存在。正常绑定不会扫描对话正文。Codex 的 `origin_leaf_id` 为空，因为 Codex thread identity 本身是 R9 v1 的 continuation unit。R9a 已实测 `submit_science_run -> submit_run_v2 -> gm00 Slurm -> terminal`，提交 MCP client 退出后计算仍独立完成；Codex terminal dispatch 留给 R9b。

terminal delivery 采用 live/offline 两层：运行中的 exact thread 优先 `codex queue --thread <id> --message <bounded completion>`；live 不可用时再以 `codex exec resume <id> --json <bounded completion>` 恢复持久 thread。offline worker 必须解析 JSONL 事件并验证实际 `thread.started.thread_id` 与 binding 完全一致；exit 0 本身不构成恢复成功。所有自动路径沿用用户现有 Codex trust/sandbox/config，禁止自动加入 dangerous approval/trust bypass flags。


## 安全边界

- 不维护第二份 Host 地址簿。
- 本地 IPC 也做主体授权：Windows 仅当前用户 SID + `SYSTEM` 且拒绝 remote clients；Unix data dir/socket 分别 `0700`/`0600`。
- 不接受所有 SSH host key。
- 不把 scheduler/user-controlled path 直接无引用拼入 shell。
- completion delivery 必须幂等，可重试，可审计。
- 自动恢复 agent 时遵守项目 trust；不能为了无人值守偷偷 `--approve` 未信任 workspace。
- 通用任意远端命令属于 agent workspace 工具，不属于 daemon lifecycle API。

## 当前迁移顺序

详细进度以 [DEVELOPMENT_CHECKPOINT.md](DEVELOPMENT_CHECKPOINT.md) 为准。架构顺序固定为：

1. single-authority durable core；
2. `Transport × Runner`；
3. pi-runs adapter migration；
4. Pi live continuation；
5. Pi offline continuation；
6. branch/rebind safety；
7. fault matrix；
8. 其它 coding agent adapters。

在 R1 完成前，旧 JSONL、直接 CLI writer、shell callback、GUI heartbeat takeover 都视为兼容实现，不再扩展其能力。
