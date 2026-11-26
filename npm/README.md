# @jixoai/agent-sync

多台 Mac 之间同步 `~/.agents` / `~/.zcode` / `~/.codex` 的提示词、skills 与指令。

- **状态**：colocated jj 仓；每台设备只写自己的 `dev-<device>`，`main` 只做本机对齐。
- **组网**：iroh（QUIC 打洞 + relay 兜底）。没有中心服务器。
- **触发**：文件变动 → 提交 → 广播「我变了」→ 对端立刻拉取（定时只兜底）。
- **形态**：一个 CLI；`install` 会把菜单栏应用注入 `~/Applications` 并注册 launchd。

## 安装

```bash
npm i -g @jixoai/agent-sync
agents-sync install      # 注入 ~/Applications/AgentsSync.app + 两个 launchd 单元
agents-sync status
```

要求：macOS（Apple Silicon）。CLI 与界面都是原生二进制，npm 只是分发通道。
装好后菜单栏会出现托盘图标：点击弹出面板，右键可立即同步 / 打开配置 / 退出
（退出只关界面，后台 daemon 继续同步）。


## 接入第二台设备

```bash
# 新设备：装好后拿 ticket
agents-sync ticket

# 任一已有设备：经 SSH 交换双方身份（幂等）
agents-sync enroll kzf@192.168.1.106
agents-sync ping <新设备 id>
```

之后两端自动双向同步；删除请两端都删（默认只增不删，见项目 README）。

## 升级

```bash
agents-sync update --check   # 只看
agents-sync update           # 下载 → sha1 校验 → 替换 → 重启单元
```

菜单栏面板也会显示新版本并提供一键更新；daemon 默认每 6 小时检查一次
（`~/.agents/sync/config.toml` 的 `[update] auto = false` 可关掉自动安装）。

## 其它命令

```
agents-sync status | sync | doctor | net | daemon | app | uninstall
```

完整说明见 [GitHub](https://github.com/jixoai/agent-sync)。
