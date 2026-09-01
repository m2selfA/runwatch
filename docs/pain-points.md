# 痛点：Windows 桌面 + 远端 HPC 上的长任务接续

本文记录为什么要做 runwatch，以及它要消除的具体断裂点。
设计见 [design.md](design.md)。

## 工作场景

- 日常机器是 **Windows 桌面**，Pi / 其它 coding agent 跑在这台机器上。
- 数据和计算在 **Linux 工作机或集群登录节点**。集群节点共享存储。
- 登录靠 `~/.ssh/config` 里已经配好的免密 Host，可能有多级 `ProxyJump`。
- 提交的是 **Slurm / LSF** 作业，也可能是远端文件哨兵或 Windows 本机的 durable Process Run；旧 PowerShell `Start-Job` 只属于迁移兼容。
- 一次科研迭代不是「提交完就结束」，而是要绕好几圈：写脚本 → 提交 → 等算完 → 读结果 → 再分析。

```text
1. agent 写分析脚本
2. agent 提交集群作业
3. 作业跑很久（分钟到天）
4. agent 读结果、继续分析
5. 进入下一轮
```

最容易断的是 **2 → 3 → 4**。人必须自己盯作业、作业结束后再把「继续」喂给 agent。这正是本项目要去掉的人工等待。

## 痛点

### 1. Agent 进程活不过作业

Pi 关了、会话断了、机器休眠了，内存里的「等 sacct」循环就没了。
集群作业还在跑，但没有人负责在终态时把分析接回去。

单独给 Pi 做插件不够：插件跟 Pi 同生共死。需要一个 **agent 关掉也还在** 的 watcher。

### 2. JobID 不是产品对象

Slurm / LSF 的 JobID 只是调度器句柄。一次科研迭代还要带上：

- 在哪台 Host 上提交（`~/.ssh/config` 别名）
- 共享盘上的项目目录和终态哨兵文件
- 成功 / 失败后要叫醒哪一个 agent、哪一次会话
- 回调时要说什么（避免叫醒错的 Pi 实例）

没有「Run」这种本地账本，插件只能记住当前对话里的一个 job。

### 3. Windows 上的 SSH 复用不可靠

OpenSSH `ControlMaster` 在 Windows 上经常是半残的。
作业轮询如果每次新开一条 SSH（再叠加 ProxyJump），既慢又容易被登录节点限连。
需要进程内保活、按 Host 复用会话，而不是依赖系统 ssh 的 control socket。

### 4. 数据在远端，agent 在本机

本机 Pi 看不到集群共享盘。提交前要在登录节点上写脚本、改输入；结束后要在同一份存储上读输出。

这里必须把两个问题拆开：

- **远端工作空间操作**：Pi 在线时由 `pi-ssh-tools` 显式提供 `ssh_read` / `ssh_write` / `ssh_edit` / `ssh_bash`；
- **Run 生命周期操作**：Pi 退出后仍需由 runwatch 的窄 SSH transport 做 submit / poll / cancel / sentinel / bounded logs。

两者共享同一个 `~/.ssh/config` Host alias 和 remote cwd 语义，但不共享连接对象，也不互相变成依赖。runwatch 不再扩张成完整远端 IDE。

### 5. 调度器不统一

同一套科研流程会碰到：

| 环境 | 句柄 |
|---|---|
| Slurm | `sbatch` / `sacct` |
| LSF | `bsub` / `bjobs` |
| 共享盘 | 作业脚本自己写的 terminal 文件 |
| 本机 Windows | `Process`：PID + creation-time 稳定句柄 + terminal sentinel |

watcher 必须把它们都映射成同一个 Run 状态机，不能只做 Slurm 插件。

### 6. 回调会叫错 agent，甚至叫对 session 也可能叫错 branch

机器上可能同时开着多个 Pi。作业结束若只是「通知 Pi」，会叫醒错误实例；而 Pi session 本身还是树结构，用户等待期间可能 `/tree`、`/fork` 或 `/resume` 到别的科研分支。

所以 continuation 需要 durable binding：至少保存 agent、session id/session file、origin leaf、项目目录和 remote workspace。终态 delivery 必须可重试、幂等，并在 branch divergence 时进入 `needs_rebind`，而不是强行注入错误上下文。

### 7. 人还要盯着窗口

即使有 CLI，人也需要：

- 开机自启、缩到托盘、关窗不等于退出
- 一眼看到有几条 live / failed
- 作业结束有本机提示
- 给其它 agent 用的 CLI / MCP，而不是只能点 GUI

## 非目标（现在不做）

- 替代 Slurm / LSF 本身。
- 在 Windows 上重做一套 Host 通讯录（只认 `~/.ssh/config`）。
- 把 watcher 绑死在某一个 coding agent 上。
- 完整实现远端 IDE / 文件浏览器（那是 ssh-tools 的职责）。

## 成功标准

一次典型循环里，人只在第一步参与「让 agent 开工」。之后：

1. agent 通过 runwatch 先持久化 Run/Attempt，再提交作业；
2. watcher（不依赖 Pi 窗口）保活 SSH、轮询终态；
3. 终态在同一 durable store 中生成 pending continuation delivery；
4. Pi 在线则投递到 live session，Pi 已退出则恢复准确的 session/branch；
5. agent 用 `pi-ssh-tools` 显式激活原 remote workspace、读取科研结果并进入下一轮。

中间不需要人把「作业好了，继续」再敲一遍。
