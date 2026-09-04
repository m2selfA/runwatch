# 设计：runwatch

对应痛点见 [pain-points.md](pain-points.md)，阶段计划与实现状态见 [DEVELOPMENT_CHECKPOINT.md](DEVELOPMENT_CHECKPOINT.md)。

## 一句话

**runwatch 是 coding agent 长科学计算的 Durable Run Lifecycle Authority。**

Run 是产品对象；SSH 连接、Slurm / LSF JobID、sentinel 文件和 agent 进程都只是句柄。runwatch 的职责不是替代调度器，也不是成为远端 IDE，而是保证 coding agent 退出后，计算仍被可靠观察，并在终态时把 continuation 可靠地交回正确的 agent 会话。

当前产品目标固定为 Windows 桌面上的 Pi coding agent + 远端 Linux HPC / Windows 本地 durable Process。只有在 `runwatch` + `pi-runs` v1 完整收口后，才重新开启其它 coding agent 的产品化；R9/R10 已验证的 Codex 路径在此之前只作为第二适配器参考证据。

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
runwatch 的 OpenSSH 配置来源默认仍是用户的 `~/.ssh/config`；如需要明确的多配置/隔离 transport，可通过 `RUNWATCH_SSH_CONFIG` 指定单一 config 文件。alias discovery 与 `ssh -G` 必须使用同一来源，且该覆盖不改变 HOME、Pi agent 配置或 known-hosts 信任策略。

### Future Agent Integration boundary

Pi 是当前唯一发布目标的 Agent Integration Plane。未来若恢复 Codex 等 agent 支持，应采用与 `pi-runs` 对称的独立集成项目（例如设计中的 `codex-runs`），由该项目持有 agent-specific session identity、resume/settlement evidence、onboarding 与 UX；`runwatchd` 最终只保留 agent-neutral `ContinuationBinding` / `Delivery` / `AgentInvocation` 契约。当前仓库中已经实现的 Codex `_meta.threadId`、rollout scanner、`codex exec resume` 与 onboarding 代码属于已验证的过渡参考实现，**当前阶段不继续扩张，也暂不创建 `codex-runs` 仓库**。

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

旧 `on_complete` / `on_success` / `on_failure` / `acked_at` callback 字段已从当前 `RunRecord`、JSON schema 与 MCP output schema 删除；serde 对旧 JSON 的未知字段保持兼容，因此一次性 `runs.jsonl -> SQLite` importer 仍可读取历史记录而不会重新暴露 callback API。新 continuation 一律进入 durable Delivery/outbox；CLI 若显式使用隐藏的旧 callback 参数会 fail closed。

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

### Human Run Console：GUI 的产品定位

v0.1 的 GUI 只证明了 tray、daemon observer、Pause 和本机通知链路，不能继续按“把 `list_runs` 拼成几行文本”的方向扩张。后续 GUI 的目标是 **Human Run Console**：让人类用户快速回答四个问题：

1. 现在有哪些 Run 在运行、排队或需要我注意？
2. 某个 Run 当前到底是什么状态，最近一次 observation 是否可信？
3. 如果出错，我能否立即看到日志、生命周期产物、attempt/continuation 线索？
4. 哪些动作是安全的人类控制动作，哪些必须回到 Pi/agent 才能做？

GUI 仍不变成第二套 scheduler、SSH IDE 或 agent runtime。它展示和控制 canonical runwatch 状态，但不拥有生命周期真相。

### GUI 信息架构

主窗口从单页状态小窗升级为 4 个一级页面，默认打开 `Runs`：

```text
Runs       默认工作台：Run 列表、搜索/过滤、attention、选中 Run 的详情
Hosts      ~/.ssh/config 的只读有效 Host 视图 + 显式诊断入口
Service    runwatchd/supervisor/Task Scheduler/版本/暂停状态
Settings   GUI 自启动、通知、显示偏好等纯 UX 设置
```

窗口默认尺寸提高到适合数据工作台的约 `1080×720`，保持可缩放/可调整大小、关闭隐藏到 tray。窄窗可以降级为列表 + drill-in，而不是强行把所有列压成不可读文本。

### Runs dashboard

顶部保持一眼可见的总体状态，但不再只显示一个字符串：

- `Active`：Submitting + Queued + Running；
- `Attention`：Failed、长期 stale/unreachable、probe error、continuation retry/needs_rebind、daemon/service 异常；
- `Recent terminal`：最近完成/失败数量；
- `Daemon`：connected / paused / unavailable。

主列表采用可搜索、可排序的数据表/动态列表。默认列：

```text
State | Name | Runner | Host | Handle | Observation | Updated/Elapsed | Continuation
```

显示规则：

- `name` 优先于 opaque `run_id`；缺名时使用稳定简短 fallback，完整 Run ID 在详情中保留；
- **Execution state 与 Observation health 必须分开显示**。例如 `Running + unreachable` 不能渲染成 Unknown，更不能把 SSH 断线误导成科研任务失败；
- 当前 Attempt 的 scheduler JobID / local handle 是次级句柄，不取代 Run identity；
- 默认排序优先 `attention -> active -> recent terminal`，而不是单纯数据库插入顺序；
- 快速过滤至少有 `Active / Attention / All`，搜索覆盖 name、run_id、job handle、host 和 workspace。
- Dashboard 数据行是**单行信息带**：虚拟表文本格统一限制为一行，空间不足时裁切而不是换行。`Updated` 保留较稳定的可读宽度；Name/Handle/Observation/Continuation 再长也不能把一条 Run 撑成两行。

### Run detail

选中 Run 后进入 master-detail 或 drill-in 详情。详情顶部固定状态、Run 名称、host/runner/handle 和安全动作；正文拆成 5 个 tab：

1. **Overview**
   - Run ID / name / status / current Attempt；
   - runner、host、workspace、job handle；
   - command（默认折叠，避免长命令淹没页面）；
   - resources；
   - created/updated/terminal 时间；
   - Observation source/health/observed_at/raw_state/reason/exit code；
   - stale 由客户端按 observed_at 推导，并明确标为 observation age。
2. **Logs**
   - stdout / stderr 分页或分段；
   - 默认沿用 daemon 的 bounded tail，不偷偷下载完整远端日志；
   - 80/200/500 行档位、Refresh、Copy、Wrap；
   - 日志读取失败只影响 Logs tab，不把 Run 状态改坏。
3. **Artifacts**
   - 展示 runwatch 生命周期 inventory：script/stdout/stderr/terminal/receipt；
   - 支持复制路径；Local Process 可在以后增加“Open containing folder”；
   - Remote artifact 内容浏览/编辑仍属于 `pi-ssh-tools`，GUI 不加入通用远端文件管理器。
4. **Timeline**
   - 按时间展示 submitted/running/observation_changed/cancel_requested/terminal/continuation_* 等不可变 Event；
   - 默认只取最近有限条，允许继续加载，不能一次把整个历史拖进 UI。
5. **Continuation**
   - 显示 agent kind、session 的脱敏/缩略 identity、Delivery 状态、retry/needs_rebind 原因；
   - `needs_rebind` 明确提示“回到对应 agent/Pi session 执行 rebind”；
   - GUI **不提供 runs_rebind 按钮**，因为 GUI 没有当前 Pi branch 的权威 identity，不能伪造安全 rebind。

### GUI 允许的人类动作

第一阶段控制面只开放能清楚解释且有 daemon authority 的动作：

- `Refresh`：刷新 dashboard 本地快照；
- `Probe now`：通过 daemon-owned `probe_run` 对单个 Run 强制 observation refresh，GUI 自己不建立 SSH；
- `Copy Run ID / Job ID / Workspace`；
- `Cancel Run`：仅非 terminal Run 可用，必须二次确认，并说明这是 cancel request，最终 Cancelled 仍由 observation 收敛；
- `Pause / Resume polling`：属于 daemon 全局控制，若存在 live Runs 必须显示明显影响说明；
- resident runtime enable/disable 放在 Service，GUI autostart 放在 Settings；两者都不与单 Run 动作混在一起。

暂不加入：GUI retry/resubmit、任意 SSH shell、远端文件编辑、scheduler admin、agent branch rebind、Delivery 强制 ack。它们要么需要新的 product semantics，要么属于其它 authority plane。

### Attention 与通知

GUI 不再把“失败数”当唯一异常信号。`RunAttention` 是一个 **可丢弃的 UI projection**，由 canonical Run + Observation + Delivery/Continuation 状态推导，至少覆盖：

- execution failed；
- observation `probe_error` / `unreachable` / stale；
- cancel requested 但尚未收敛；
- continuation retrying / needs_rebind；
- daemon unavailable / polling paused；
- resident service 配置与实际 daemon health 不一致。

通知遵循“状态转移触发、当前 dashboard 为真相源”的原则：

- 成功 completion：普通通知；
- failure、needs_rebind、持续 observation loss：attention 通知；
- 同一次 poll 多个 Run 终态要合并/逐项记账，不能像 v0.1 `find_map()` 那样只通知第一个；
- heartbeat/每次 refresh 不发通知；
- GUI 重启后从 daemon 重建当前 dashboard，不把本地 notification cursor 当 lifecycle authority；
- 可保存一个纯 UX `last_seen_event`/notification preference，用于避免重复 toast，这些数据丢失也不能影响 Run/Delivery。

Tray tooltip/menu 也应反映当前状态，例如 `runwatch · 3 active · 1 attention`；只有真正 idle 时才保持简洁。Tray 是“回来时快速意识到还有任务”的入口，而不是完整控制面。

### Hosts page

Host 仍只来自 `~/.ssh/config`，不建立第二通讯录。页面展示：alias、effective destination、user/port、ProxyJump 摘要、是否被当前 live Run 使用。Host 配置读取错误必须显式显示，不能像 v0.1 `unwrap_or_default()` 那样退化成“似乎没有 Host”。

后续可增加显式 `Test connection` / trust diagnostics，但它必须通过 daemon/runwatch-ssh 的同一 fail-closed trust policy，并且只能由用户主动触发，不能因为打开 Hosts 页面就并发登录所有机器。

### Service page

Service 页面把当前散落在 tray menu 的运行维护信息集中起来：

- daemon connected / pid / version / protocol / capabilities；
- daemon paused；
- resident supervisor/Task Scheduler enabled；
- GUI logon autostart enabled；
- package sibling/version 健康；
- 最近 daemon 连接错误。

关闭 resident runtime 不等于取消科研 Run，但会停止本机 observation/continuation，若存在 live Runs 必须在确认框中明确说明这一影响。

GUI 登录自启动与 daemon resident service 继续保持两个独立设置：`Start GUI with Windows` 仅控制 UI，`Keep runwatchd running` 才控制 Task Scheduler/supervisor service。Task Scheduler XML 在已验证的 Windows 环境中必须写成 UTF-16LE + BOM；canonicalized Win32 verbatim paths还必须在进入 scheduler/cmd 前规范化成普通 DOS/UNC 路径。Task registration 还必须显式给当前用户 SID、`SYSTEM` 与 `Administrators` 管理权限，避免“提升权限安装、普通桌面 GUI 无权管理”的 split-token 陷阱。删除 resident runtime 时先 Disable registration 封住 PT1M/Logon 新触发，再 End/等待已验证 owner 退出并 Delete；中途失败则 best-effort 重新 Enable，不能留下静默 disabled 的半状态。发布门禁不仅做 create/query-XML/delete，还必须真实启动 supervisor、强杀 child 验证秒级恢复、强杀 supervisor 验证下一次 reconcile 恢复，并确认 Job Object 没有留下孤儿 child。

### GUI 数据与 IPC 设计

不要让 UI 为了做详情页重新打开 SQLite。扩展 daemon 的 **只读 IPC projection**，优先形成通用而非 GUI 专属的读模型：

```text
list_runs                 继续保留兼容
get_run                   继续保留兼容
list_run_events           新增：bounded recent Event timeline
get_continuation_status   新增：Run-scoped binding/Delivery attention projection
get_attempt               新增：当前/指定 Attempt metadata
probe_run                 新增：daemon-owned explicit single-Run refresh
```

Dashboard 可以继续一次获取 `runs + observations`，但 GUI view-model 必须把 Run/Observation 关联起来；详情信息按选中 Run 懒加载，Logs/Artifacts 单独按需读取，避免每 2 秒把所有日志/事件一起拉回来。

Delivery/Continuation projection 只暴露 GUI 需要的状态和可操作错误，不应把 session 文件内容、credential 或内部 lease token 暴露给桌面 UI。

### GUI 应用层结构

当前 `main.rs` 同时承担 tray、IPC、轮询、projection、notification 和渲染，后续必须先拆层再加功能：

```text
runwatch-gui/
  app.rs / controller.rs        UI 生命周期与 action dispatch
  ipc_client.rs                 bounded daemon IPC
  model.rs                      GuiSnapshot / RunRow / RunDetail / Attention
  projection.rs                 canonical data -> pure view-model
  notifications.rs              transition/dedupe/coalesce
  settings.rs                   仅 GUI UX 设置
  views/
    runs.rs
    run_detail.rs
    hosts.rs
    service.rs
    settings.rs
```

只保留一个长期 background controller/runtime；UI action 通过 command channel 发送，不再像当前 Pause toggle 那样每次点击临时创建线程 + Tokio runtime。轮询产生 immutable-ish snapshot，再由 WindUI signals 更新动态列表/详情。

WindUI 0.14 已具备 dynamic list、sortable table、tabs、dialog、segmented/nav、progress、clipboard 和 off-screen screenshot，先用现有框架完成上述 console；只有在真实实现中证明大数据表、动态 master-detail 或 accessibility 被框架卡住，才评估迁移，而不是因为 v0.1 UI 简陋就先换框架。

### R13 implementation note（2026-09-04）

The first Human Run Console implementation now follows the architecture above. `runwatch-gui` uses one long-lived controller/runtime; `ipc_client` is the only GUI daemon-access layer; `model` is a pure projection layer; `notifications` owns transition dedupe; and the Runs view uses a virtualized table plus lazy detail tabs. The daemon exposes additive bounded `get_attempt`, `list_run_events`, `get_continuation_status` and `probe_run` operations; `list_runs` keeps its existing `runs + observations` fields and adds a continuation sidecar rather than creating a GUI-only database path.

`probe_run` is intentionally daemon-owned. It selects one Run for execution observation through the same engine/HostPool path as ordinary ticks; the GUI never creates an SSH connection. Existing daemon terminal-Delivery reconciliation still runs as canonical maintenance and is not a GUI-owned scheduler loop.

The implemented dashboard keeps execution and observation independent, adds continuation/cancel attention, and supports Active/Attention/All filtering, search and explicit Priority/Newest/Name/Host ordering on a virtual table. Its text cells are capped at one rendered line (`cell_lines(1)`), and the Runs table/detail workspace now share vertical space through flex rather than a fixed 280px table height, so the default 1080×720 window keeps both the row band and detail content usable. Detail tabs keep Logs and Event history bounded (logs <=500 lines per request, recent events <=200) and localize partial failures. Continuation projection deliberately omits session files, origin-leaf IDs, adapter paths and lease/owner internals.

Service authority is also kept separate from Settings: resident Task Scheduler/supervisor control belongs on Service and requires confirmation before disabling; Settings contains only desktop UX choices. The installed Task carries an explicit current-user/SYSTEM/Administrators security descriptor so a normal limited desktop GUI can manage a Task even when installation happened from an elevated process. Removal first quiesces the registration before stopping/deleting the supervisor and rolls back Enable on failure, closing the one-minute trigger resurrection race. Hosts remains a read-only OpenSSH projection plus live Run usage computed from daemon snapshots; merely opening the page does not connect to every Host.

One concrete WindUI 0.14 limitation is now proven rather than hypothetical: its public tray API does not expose a runtime tray handle/tooltip mutation surface, and native `notify()` is available only inside tray callback context, not from the background controller/app-channel `EventCtx`. Therefore R13 does **not** duplicate a second tray icon or discover private Win32 handles merely to implement a dynamic tooltip/background native notification. Transition attention is currently represented by the dashboard plus in-app toasts; a clean upstream/public tray-notification bridge is the remaining R13d UI-framework item.

Screenshot fixtures are a debug/CI surface only (`debug_assertions`). Release builds do not compile the `RUNWATCH_GUI_FIXTURE` trigger, so production cannot be switched to synthetic dashboard data by environment configuration.

Independent review additionally tightened two asynchronous/error boundaries before 0.2.0 packaging: detail requests carry a generation and the controller aborts superseded loads, so slower A/B selections or older log-tail requests cannot overwrite the current detail; and Attempt/Timeline/Continuation read/parse failures render explicit local `unavailable` states rather than masquerading as absent canonical data. Because R13 changes runtime IPC plus the shipped GUI materially, current workspace/package identity is 0.2.0 while the historical v0.1.0 tag/evidence remains untouched.

### R14 manual Run submission note（2026-09-04）

Once the R13 console was stable, manual Run authoring became the next safe writable surface because it can reuse an authority that already exists instead of inventing new lifecycle semantics. The desktop GUI now builds a normal `SubmitRunSpec` and calls daemon `submit_run_v2`; it never submits directly to SSH/schedulers and never writes SQLite. This path supports the daemon's existing `Process`, `Slurm` and `Lsf` runners only.

A GUI-authored Run is intentionally **unbound**: `continuation=None` is unconditional. The GUI has no authoritative Pi branch or Codex thread identity, so allowing it to fill `ContinuationBinding` would recreate the exact authority confusion that the R13 Continuation tab avoids. If a human wants automatic agent continuation, the Run must still originate through that agent's integration surface.

The form performs only ergonomic normalization: optional human name generation, a bounded ASCII manual Run ID, positive integer parsing for CPU/GPU counts, and Slurm `partition` versus LSF `queue` mapping. All security/execution validation remains daemon-owned, including exact OpenSSH Host resolution, remote absolute workspace rules, Local Process existing-directory checks, resource-family constraints, durable submission intent, receipt idempotency and scheduler/process launch. The New Run control is hidden behind the daemon-advertised `submit_run_v2` capability and becomes unavailable on disconnect.

Remote scheduler authoring also calls out a practical filesystem invariant in the UI: the selected workspace must persist and normally be shared between the login host and compute nodes. Real R14 acceptance demonstrated the failure mode cleanly: a `/tmp` Slurm smoke reached `succeeded`, but login-side log retrieval could not see compute-node-local stdout; repeating the same smoke under `/share/home/shark` closed the full submission/terminal/log loop.

### GUI 验收基线

R13 GUI 改造至少需要以下门禁：

- 0 / 1 / 50 / 500 Runs 的 projection 与列表排序/filter/search 测试；
- 默认 1080×720 下每个 dashboard Run 行的所有文本列必须保持单行；超长 Updated/Continuation/Observation 等只能裁切，不能换行污染相邻行；
- Running + unreachable 必须显示“仍 Running，但 observation unhealthy”，不能降成 Unknown；
- 同时多个 terminal transition 不丢通知；
- daemon offline / reconnect / paused 的 dashboard 与 tray 状态一致；
- Run detail 在 logs/artifacts/event IPC 失败时局部降级，主列表仍可用；
- Cancel 必须 confirmation + terminal disabled + daemon-side cancel semantics；
- needs_rebind 只显示 attention/instruction，GUI 无法直接伪造 rebind；
- Hosts parse 失败显式可见，不静默变空；
- Window hide/show、tray、GUI/service autostart 仍保持现有语义；
- Windows packaged interactive acceptance 必须在普通用户桌面会话里验证 Service disable/re-enable、跨 PT1M 不复活、Run drill-in/log/probe/cancel 与 WM_CLOSE hide；Task 若由 elevated 安装，普通用户仍必须具有管理该 registration 的权限；
- 所有 GUI 启动/操作路径继续保证 Windows 无 console flash；
- Windows CI 除 Rust test/package 外增加关键 GUI fixture 的 off-screen screenshot smoke，至少覆盖 Runs dashboard、Run detail、daemon offline 三种状态。

## MCP

MCP 是通用协议面，不是 Pi v1 主链路。Pi 首发固定走 `pi-runs -> local IPC -> runwatchd`。MCP server 已迁移到官方 Rust SDK `rmcp 3.1.4`：通过 `server/discover` 支持 2026-07-28，同时保留 SDK 自身的 legacy initialize negotiation；当前工具均有 typed `outputSchema` 和 structuredContent。R9 添加的 `submit_science_run` / Codex `_meta.threadId` 绑定属于第二适配器实验形成的过渡 surface，不应被当成未来所有 agent 都要嵌入 runwatch-mcp 的模板。`wait_run <= 60s` 是普通同步工具，更长等待只有客户端声明 `io.modelcontextprotocol/tasks` 时才 materialize 为 MCP Task，且 Task cancel 只取消 wait handle，不取消科研 Run。这样 MCP Tasks 是 transport-level observation lifecycle，不是第二份 Run runtime。

### Codex CLI reference adapter（R9/R10，v1 冻结）

Codex 不复制 `pi-runs`。它优先复用 MCP 作为 agent binding surface：Codex app-server 在 tool call 的 `_meta.threadId` 注入真实 thread id。rmcp 3.1.4 negotiated dispatch 会先把 request `_meta` 从 `CallToolRequestParams` 取出并放进 `RequestContext.meta`，因此 runwatch-mcp 在进入通用 tool router 前从 **`RequestContext.meta`** 提取 `threadId`。模型参数里不出现 `thread_id`，从而消除“模型把 Run 绑定到错误会话”的自由度。

R9 的 binding 来源是两份互相校验的本地权威元数据：

```text
MCP _meta.threadId  ---- exact identity ----┐
                                            ├─> ContinuationBinding(agent_kind=codex)
~/.codex/sessions/**/rollout-*.jsonl        │
first record: session_meta { id, session_id, cwd } ┘
```

session locator 只读取 rollout 第一条 `session_meta`，要求 `payload.id == _meta.threadId`，且真实 0.150.1 中若同时存在 `payload.session_id` 则进一步要求 `session_id == id`；取 `payload.cwd` 作为 project root，并要求该目录仍存在。正常绑定不会扫描对话正文。Codex 的 `origin_leaf_id` 为空，因为 Codex thread identity 本身是 R9 v1 的 continuation unit。R9a 已实测 `submit_science_run -> submit_run_v2 -> gm00 Slurm -> terminal`，提交 MCP client 退出后计算仍独立完成；R9b/R9c 随后完成 offline exact-thread resume 与 rollout-backed idempotency。

R9b 已实现可靠的 **offline** Codex driver：daemon 为 `agent_kind=codex` 独立 reserve session lease，然后以 native `codex.exe exec resume --json <thread_id> <bounded completion>` 恢复持久 thread。completion 带 deterministic `delivery_id` marker；stdout 采用 bounded JSONL parser，只有 exact `thread.started.thread_id` + `turn.started` + `turn.completed` + process exit 0 且无 failure/error/malformed event 才能把 Delivery 标为 delivered。缺失/替换 thread identity 直接 `needs_rebind`；普通 agent/provider 失败走 retry。单条超大 item 输出被 drain 后丢弃，stderr 只保留 bounded tail，避免科研输出撑爆 resident daemon。

R9c 不把 `codex queue` 作为 correctness dependency，而是直接使用 Codex 0.150.1 的**持久 rollout 证据**收紧并发和 exactly-once。真实 rollout 中每个 agent turn 有 `event_msg/task_started {turn_id}`，用户输入持久为 `response_item/message role=user` 的 `input_text`，完成边界是同一 `turn_id` 的 `event_msg/task_complete`。runwatch 的 deterministic Delivery marker 正好位于该 user input，因此 daemon 重启后可区分：marker 不存在且 thread idle、marker 已持久但 turn 仍在运行、marker turn 已持久完成。

spawn 前 scanner 只寻找这些结构和本 Delivery marker，不保存一般对话文本：其它 active turn -> retry；同 marker active turn -> retry 且绝不重注入；同 marker + matching task_complete -> **不启动 Codex，直接补 durable delivered ack**；重复 marker、无 turn marker 或不一致历史 -> needs_rebind。只有 rollout 已 idle 且静默至少 3 秒才允许新 `exec resume`。因此“原 Codex CLI 正在 active turn”与“Codex 已完成但 daemon 在 SQLite ack 前崩溃”两条关键竞态都被持久层封住。`codex queue` 仍可作为未来 resident app-server latency 优化，但 queue exit 0 本身永远不能等价于 Delivery success。所有自动路径沿用用户现有 Codex trust/sandbox/config，禁止自动加入 dangerous approval/trust bypass flags。

R9d 已完成真实 provider 终验：Codex 0.150.1 exact thread `01a05b2b-4cba-7632-9be0-3f3cc18ca3ab` 通过实际 MCP `_meta.threadId` 提交 gm00 Slurm Job 31739 后完全退出；runwatchd 在 Run terminal 后自动 `exec resume` 同一 thread，SQLite Delivery=`delivered`/AgentInvocation=`completed`，rollout 中 deterministic marker 恰好一次且所在 turn 有 matching `task_complete`。整个恢复过程没有人工 `continue`。R10a/R10b 又完成了保守 onboarding 与只读 doctor。到这里第二适配器实验已经达到目的；当前 v1 周期冻结后续 Codex 产品化。

### Codex onboarding experiment（R10a/R10b 已完成，后续冻结）

Codex adapter 的安装权威仍然是 **Codex 自己的 MCP 配置管理面**，不是 runwatch 直接改 `config.toml`。`runwatch agent codex install/status/remove` 通过 `codex mcp get/add/remove` 管理固定名称 `runwatch`，并把当前 `runwatch` 同目录的 `runwatch-mcp` 作为唯一 owned target。

所有权判定是保守的：只有 `stdio + 无额外 args + command path 精确匹配 sibling runwatch-mcp` 才视为 runwatch 自己安装的 entry。不存在时 install/remove 幂等；同名但 transport/command/args 不匹配时进入 `conflict`，install 与 remove 都拒绝覆盖或删除。这样 runwatch 不会把用户已有的同名 MCP 当成自己的配置。

Windows 对外部 consumer 写入路径前统一去除 Win32 verbatim prefix：`\\?\C:\... -> C:\...`、`\\?\UNC\server\share\... -> \\server\share\...`。这是 R10a 隔离 Codex 0.150.1 实机 round-trip 抓到的真实问题，沿用 R8 Task Scheduler 已验证的 consumer-safe path 原则。Codex 管理子进程使用 native `codex.exe`/显式 `RUNWATCH_CODEX_EXECUTABLE` 且 `CREATE_NO_WINDOW`，不依赖 PowerShell shim。

R10b 的 `runwatch agent codex doctor` 是纯只读 readiness 聚合，不是安装入口。它同时验证 native launcher、sibling `runwatch-mcp`、sessions root 形态、owned+enabled MCP registration 与 daemon hello 中的 `offline_codex_continuation` capability，并把 `daemon unreachable` 与 `daemon incompatible` 分开报告。首次使用尚未生成 sessions 目录不会被误判为不支持；真正需要无人值守恢复时，ephemeral/缺失 rollout 仍由 R9 continuation preflight fail closed。**R10c 及进一步 Codex release work 现已延后到 Pi-first v1 之后。** 届时先设计/提炼 agent-neutral adapter contract，再决定如何把这些 Codex-specific 机制迁入独立 Agent Integration 项目；当前不新建该项目。


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
8. **Pi-first v1 release closure：distribution / install-readiness / repeatable real-Pi acceptance / multi-hour soak / compatibility retirement**；
9. v1 完成后才重新评估 agent-neutral adapter extraction 与其它 coding agent integration projects。

R1–R8 的 durable core 已基本收口；当前不要为了未来 agent 扩张 runwatch 的 agent-specific surface。旧 JSONL、历史 callback 和其它迁移兼容只允许为 v1 迁移/回滚保留，不再扩展能力。
