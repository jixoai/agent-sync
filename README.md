# agents-sync

多台 Mac 之间同步 `~/.agents` / `~/.zcode` / `~/.codex` 的提示词、skills、指令与文档。

- **状态版本化**：colocated jj 仓（`~/.agents/sync`）。每台设备只写自己的
  `dev-<device>` bookmark；`main` 只是本机对齐基线，从不推到远端。
- **组网**：iroh（QUIC + 打洞 + relay 兜底）。gaubee-cloud 只做网络，不存任何状态。
- **事件驱动**：文件变动 → 采集 → 提交 → 广播「我变了」→ 对端立刻拉取。
  定时 tick 只兜底丢失的广播。
- **唯一入口**：一个 CLI。`install` 会把菜单栏应用注入 `~/Applications` 并注册 launchd。

## 组成

```
crates/core    采集 / 落盘 / 符号链接 / jj 与 git / 配置模型
crates/net     iroh 节点：TCP 转发（SSH 走它）、广播、三级地址链
crates/daemon  FSEvents 监听 + unix socket IPC + 串行同步 + 广播
crates/cli     argv 分派、install/uninstall、IPC 客户端、enroll
app/           nsh (TSX) 菜单栏面板 + 原生模块 asui/native
```

## 命令

| 命令 | 作用 |
|---|---|
| `agents-sync install` | 注入 `~/Applications/AgentsSync.app`，注册 UI 与 daemon 两个 launchd 单元 |
| `agents-sync install-status` | 查看注入状态 |
| `agents-sync status` | 汇总状态；daemon 在跑就返回实时值（JSON） |
| `agents-sync sync` | 立即同步一次（交给 daemon，避免两路同时写活目录） |
| `agents-sync ticket [--json]` | 本机 iroh ticket |
| `agents-sync enroll <user@host>` | **经 SSH 交换双方身份**，identity 漂移后的一条命令自愈 |
| `agents-sync ping <peer-id>` | 单独验证「能不能叫醒对端」 |
| `agents-sync net` | 组网诊断（本机身份 + 各 peer 的 ticket/端口/局域网地址） |
| `agents-sync doctor` | 只读体检 |
| `agents-sync daemon` | 无界面常驻（launchd 用的就是它） |
| `agents-sync app` | 前台运行菜单栏应用（内部自带 daemon 线程） |

## 配置

配置是**本机文件，不入库**（模板由程序按需生成）：

- `~/.agents/sync/config.toml`：行为与目录映射（`[sync]`、`[net]`、`[[trees]]`）
- `~/.agents/sync/config.local.toml`：本机覆盖（UI 的开关与参数写这里）

为什么模板不入库：同一份 `config.toml` 进版本库后，两端各自的修改会在
jj merge 里永远打架（实测 `prune` 值被来回翻转）。共享的是**内容**
（payload），不是每台机器的行为参数。

```toml
[sync]
tick_interval = 1800   # 兜底轮询（秒）
watch_cooldown = 15    # 编辑器保存合并窗口（秒）
backup_keep = 20       # 落盘前快照保留份数
prune = false          # 关闭：只增不删（推荐）；开启＝镜像语义，有误删风险

# [net]
# bind_port = 22401
# serve_target = "127.0.0.1:22"

# [[trees]]            # 目录映射，默认等价于 agents / zcode / codex 三棵
# name = "agents" ...
```

本机暂停某棵树（UI 的开关）：

```toml
[local.tree_enabled]
codex = false
```

暂停的树既不上传也不落盘，payload 里既有内容保留，重新启用不会丢。

设备清单（ticket / 端口 / 局域网地址）在 `config/devices.json`，每台机器各写各的，
故意不入库——它进 payload 只会制造 2-sided 冲突。

## 删除语义（重要）

**默认只增不删**：payload 是各设备内容的并集。

独立快照 + jj 三方合并里，「一端删除、另一端把相同内容恢复回来」与「从未改动」
不可区分——删除会反复复活或误删（实测丢过 103 个文件，两端来回抖动）。
因此：

- 采集只把活目录的内容写进 payload，**从不因为「本机没有」删 payload**（否则
  等于替对端声明删除）。
- 落盘默认也不删（`prune = false`）。开启 `prune` 即接受镜像语义的风险，
  写入前的快照（`backups/`，保留 `backup_keep` 份）仍可回滚。
- 想真正清掉某个文件：在两台机器上都删掉即可（它不再出现在任何采集集里，
  不会被子后续对齐复活；payload 里的旧副本可等 `prune` 或手动清理）。

`not_after` 竞态护栏始终生效：同步窗口内新建/编辑的文件留到下一轮。

## 首次接入一台新机器

```bash
# 新机器
agents-sync install                     # 注入应用与 launchd
agents-sync ticket                      # 拿到本机 ticket

# 任一已有机器
agents-sync enroll kzf@<新机器IP>        # 双向写入 devices.json 并重启两端
agents-sync ping <新设备id>              # 应当 ok
```

`enroll` 是幂等的：端口、路径等人工配置不会被改动，只对齐身份。

## 已知边界

- **payload 里的嵌套 `.gitignore` 会影响跟踪**：jj/git 尊重 `.gitignore`，若某个
  skill 自带 `.gitignore`（例如 `img2threejs` 忽略了 `package-lock.json`），
  被它忽略的文件不会进仓库、也就不会同步。这是 git 语义，不是本工具的 bug；
  要同步这类文件，删掉对应 `.gitignore` 或改规则。
- **删除不传播**（见上）：`prune = false` 是默认与推荐值。
- UI 是菜单栏窗口（`ActivationPolicy::Accessory`，无 Dock 图标）；nsh 的
  `hidden` 标题栏会保留窗口左上角的红绿灯按钮，这是 AppKit 的行为。

## 故障排查

| 现象 | 原因 | 处理 |
|---|---|---|
| 隧道「连上了但等不到 SSH banner」 | 对端 ticket 过期（`.secrets` 被重建过） | `agents-sync enroll <user@host>` |
| `ping` 成功但同步不收敛 | 看 `status` 的 `failed_fetch` / `conflicts` | `doctor` |
| 两端每 20 秒空转、历史暴涨 | 无条件广播的回声 | 已修：仅在本机产生新提交时广播；align 已包含则不造 merge |
| 首次运行弹「允许查找本地网络设备」 | macOS 本地网络权限 | 允许（iroh 打洞需要） |

## 测试

```bash
cargo test --workspace        # core / net / daemon / cli
tests/two_home.sh             # 双 HOME 双向同步（无网络依赖）
tests/cross_machine.sh        # 真机：iroh 隧道 + 真实 sshd + jj
```

`app/` 是独立的 nsh 工程：`nsh build` / `nsh run --window-size 380x500` /
`nsh package --target macos`。
