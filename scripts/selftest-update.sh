#!/usr/bin/env bash
# 自更新 + npm 启动器端到端自测：本地假 GitHub Release，不联网、不发版。
#
#   scripts/selftest-update.sh
#
# 覆盖：
#   1. Rust 更新器：取 SHA256SUMS → 版本比较 → 下载 → sha256 → 解包 → 替换 → 调 install
#   2. npm 启动器：首次运行下载 → 校验 → 解包 → 缓存 → 执行
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=${BIN:-target/release/agents-sync}
[ -x "$BIN" ] || { echo "先构建：cargo build --release -p agents-sync"; exit 1; }

WORK=$(mktemp -d)
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
trap 'kill ${SRV_PID:-0} 2>/dev/null; rm -rf "$WORK"' EXIT

V=0.9.9
BASE="http://127.0.0.1:$PORT/releases"

# ---- 假产物：一个会自报版本的 shell 脚本 ----
mkdir -p "$WORK/pkg"
cat > "$WORK/pkg/agents-sync" <<'EOF'
#!/bin/sh
case "$1" in
  --version) echo "agents-sync 0.9.9" ;;
  install) echo "fake-install" ;;
  *) echo "fake $*" ;;
esac
EOF
chmod 755 "$WORK/pkg/agents-sync"
( cd "$WORK/pkg" && tar -czf "$WORK/agents-sync-v$V-darwin-arm64.tar.gz" agents-sync )

# ---- 假 Release 目录结构（与 GitHub 的 URL 形状一致）----
REL="$WORK/releases/download/v$V"
mkdir -p "$REL" "$WORK/releases/latest/download"
cp "$WORK/agents-sync-v$V-darwin-arm64.tar.gz" "$REL/"
( cd "$REL" && shasum -a 256 ./*.tar.gz > SHA256SUMS.txt )
cp "$REL/SHA256SUMS.txt" "$WORK/releases/latest/download/SHA256SUMS.txt"

# ---- 静态服务器 ----
( cd "$WORK" && python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1 ) &
SRV_PID=$!
sleep 0.7

echo "== 1. Rust 更新器 =="
export HOME="$WORK/home"
mkdir -p "$HOME/.local/bin" "$HOME/Applications"
printf '#!/bin/sh\necho old\n' > "$HOME/.local/bin/agents-sync"
chmod 755 "$HOME/.local/bin/agents-sync"
OUT=$(AGENTS_SYNC_RELEASE_BASE="$BASE" AGENTS_SYNC_MIRROR=direct AGENTS_SYNC_NO_RESTART=1 \
      "$PWD/$BIN" update 2>&1)
echo "$OUT"
grep -q "已更新 0" <<<"$OUT"
test "$("$HOME/.local/bin/agents-sync" --version)" = "agents-sync 0.9.9" || { echo "CLI 未替换"; exit 1; }
test "$("$HOME/.local/bin/agents-sync.old")" = "old" || { echo "旧版本未备份"; exit 1; }

echo "== 2. npm 启动器 =="
rm -rf "$WORK/npmtest"
mkdir -p "$WORK/npmtest"
cp -R npm/bin npm/lib "$WORK/npmtest/"
V="$V" node -e '
  const fs = require("fs");
  const p = JSON.parse(fs.readFileSync("npm/package.json", "utf8"));
  p.version = process.env.V;
  fs.writeFileSync(process.argv[1], JSON.stringify(p, null, 2));
' "$WORK/npmtest/package.json"
AGENTS_SYNC_RELEASE_BASE="$BASE" AGENTS_SYNC_MIRROR=direct \
AGENTS_SYNC_CACHE="$WORK/cache" node "$WORK/npmtest/bin/agents-sync.js" --version \
  | grep -q "agents-sync 0.9.9"
test -x "$WORK/cache/agent-sync/v$V/agents-sync" || { echo "启动器未缓存二进制"; exit 1; }

echo "✓ 两条链路都通过（更新器 / npm 启动器）"
