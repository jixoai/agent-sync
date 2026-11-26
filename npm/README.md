# @jixoa/agent-sync

多台 Mac 之间同步 `~/.agents` / `~/.zcode` / `~/.codex` 的提示词、skills 与指令。

**这个 npm 包只有几 KB 的 JS 启动器，不含二进制。** 首次运行时按「包版本 + 平台」
从 GitHub Release 下载 `agents-sync-v<ver>-darwin-arm64.tar.gz`，校验 SHA256 后缓存到
`~/Library/Caches/agent-sync/v<ver>/`，之后直接执行缓存里的二进制。

## 安装

```bash
npm i -g @jixoa/agent-sync
agents-sync install      # 注入 ~/Applications/AgentsSync.app + 两个 launchd 单元
agents-sync status
```

要求：macOS（Apple Silicon）。

## 国内加速

GitHub Release 直连经常超时。默认按顺序尝试社区镜像（gh-proxy.com → ghfast.top →
ghproxy.net → 直连）；也可以自己指定：

```bash
AGENTS_SYNC_MIRROR="https://your.mirror/" npm i -g @jixoa/agent-sync
AGENTS_SYNC_MIRROR="https://your.mirror/" agents-sync   # 运行/更新时同理
```

`direct` 表示直连。这个变量与二进制自身的更新器共用同一套约定。

## 接入第二台设备

```bash
agents-sync ticket                 # 新设备拿 ticket
agents-sync enroll kzf@192.168.1.106   # 任一已有设备：经 SSH 交换双方身份
agents-sync ping <新设备 id>
```

## 升级

```bash
agents-sync update --check   # 只看
agents-sync update           # 下载 → sha256 校验 → 替换 → 重启单元
```

菜单栏面板也会显示新版本并提供一键更新；daemon 默认每 6 小时检查一次
（`~/.agents/sync/config.toml` 的 `[update] auto = false` 可关）。

其它命令：`status | sync | doctor | net | daemon | app | uninstall`。
完整说明见 [GitHub](https://github.com/jixoai/agent-sync)。
