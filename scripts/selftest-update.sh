#!/usr/bin/env bash
# 自更新端到端自测：本地假 registry + 临时 HOME，不联网、不发版。
#
#   scripts/selftest-update.sh
#
# 覆盖：版本比较 → 元数据解析 → 下载 → sha1 校验 → 解包 → 原子替换 CLI → 换 .app
#      → 旧版本留 .old。故意不重启 launchd（AGENTS_SYNC_NO_RESTART=1）。
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=${BIN:-target/release/agents-sync}
[ -x "$BIN" ] || { echo "先构建：cargo build --release -p agents-sync"; exit 1; }

WORK=$(mktemp -d)
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')
trap 'kill ${SRV_PID:-0} 2>/dev/null; rm -rf "$WORK"' EXIT

# ---- 造一个「新版本」包：真的二进制换成脚本，方便断言替换发生 ----
mkdir -p "$WORK/build/package/bin" "$WORK/build/package/lib/AgentsSync.app/Contents"
PKG="$WORK/build/package"
cat > "$PKG/bin/agents-sync" <<'EOF'
#!/bin/sh
echo "fake-0.1.1"
EOF
chmod 755 "$PKG/bin/agents-sync"
printf '<?xml version="1.0"?><plist><dict><key>CFBundleShortVersionString</key><string>0.1.1</string></dict></plist>\n' \
  > "$PKG/lib/AgentsSync.app/Contents/Info.plist"
printf '{"name":"@jixoa/agent-sync","version":"0.1.1"}\n' > "$PKG/package.json"
( cd "$WORK/build" && tar -czf "$WORK/pkg.tgz" package )

SUM=$(shasum -a 1 "$WORK/pkg.tgz" | awk '{print $1}')
cat > "$WORK/registry.py" <<'PY'
import http.server, sys, json
meta_path, tgz_path = sys.argv[1], sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        # 用原始 path 精确匹配：%2F 不能被解开
        if self.path == "/@jixoa%2Fagent-sync/latest":
            body = open(meta_path, "rb").read()
            self.send_response(200); self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body))); self.end_headers()
            self.wfile.write(body)
        elif self.path == "/pkg.tgz":
            body = open(tgz_path, "rb").read()
            self.send_response(200); self.send_header("content-type", "application/octet-stream")
            self.send_header("content-length", str(len(body))); self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404); self.end_headers()
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", int(sys.argv[3])), H).serve_forever()
PY
python3 - "$WORK" "$SUM" "$PORT" <<'PY'
import json, sys
work, shasum, port = sys.argv[1], sys.argv[2], sys.argv[3]
json.dump({
    "name": "@jixoa/agent-sync",
    "version": "0.1.1",
    "dist": {"tarball": f"http://127.0.0.1:{port}/pkg.tgz", "shasum": shasum},
}, open(f"{work}/latest.json", "w"))
PY

python3 "$WORK/registry.py" "$WORK/latest.json" "$WORK/pkg.tgz" "$PORT" &
SRV_PID=$!
sleep 0.7

# ---- 临时 HOME：预先放一个「旧版本」CLI 与 app ----
export HOME="$WORK/home"
mkdir -p "$HOME/.local/bin" "$HOME/Applications/AgentsSync.app"
printf '#!/bin/sh\necho old\n' > "$HOME/.local/bin/agents-sync"
chmod 755 "$HOME/.local/bin/agents-sync"
echo "旧 app" > "$HOME/Applications/AgentsSync.app/marker"

echo "→ 查询（应看到 0.1.0 → 0.1.1）"
OUT=$(HOME="$HOME" AGENTS_SYNC_REGISTRY="http://127.0.0.1:$PORT" \
      AGENTS_SYNC_NO_RESTART=1 "$PWD/$BIN" update 2>&1)
echo "$OUT"

echo "→ 断言"
grep -q "已更新 0.1.0 → 0.1.1" <<<"$OUT"
test -x "$HOME/.local/bin/agents-sync"
test "$("$HOME/.local/bin/agents-sync")" = "fake-0.1.1" || { echo "CLI 未替换"; exit 1; }
test "$("$HOME/.local/bin/agents-sync.old")" = "old" || { echo "旧版本未备份"; exit 1; }
test -f "$HOME/Applications/AgentsSync.app/Contents/Info.plist" || { echo "app 未替换"; exit 1; }
test -f "$HOME/Applications/AgentsSync.app.old/marker" || { echo "旧 app 未备份"; exit 1; }
echo "✓ 自更新全流程通过（下载 / sha1 / 解包 / 替换 / 备份）"
