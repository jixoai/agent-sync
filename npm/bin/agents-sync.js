#!/usr/bin/env node
// npm 只是一层启动器：真正的二进制从 GitHub Release 下载。
//
// 参考 reply9842 的分发形态：
//   · npm 包只有几 KB（bin/ + lib/），安装秒级完成；
//   · 首次运行按「版本 + 平台」下载 `agents-sync-v<ver>-darwin-arm64.tar.gz`，
//     校验 SHA256 后缓存到 ~/Library/Caches/agent-sync/，以后直接 exec 缓存；
//   · 二进制里已嵌好菜单栏 app，`agents-sync install` 会把它解出来。
require("../lib/run.js");
