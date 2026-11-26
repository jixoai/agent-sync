#!/usr/bin/env bash
# 本地组装发布产物（与 CI 同一套步骤），用于试跑发布/自更新，不联网。
#
#   scripts/stage-npm.sh [版本号]
#
# 产物：
#   dist/rel/agents-sync-v<ver>-darwin-arm64.tar.gz  + SHA256SUMS.txt（GitHub Release 资产）
#   dist-npm/（只有 JS 启动器 + package.json）
set -euo pipefail
cd "$(dirname "$0")/.."

V="${1:-$(node -p "require('./npm/package.json').version" 2>/dev/null || echo 0.0.0)}"
echo "→ 版本 $V"

( cd app && nsh build && nsh package --target macos )
mkdir -p dist/rel
tar -czf dist/AgentsSync.app.tar.gz -C app/.nsh/out AgentsSync.app
AGENTS_SYNC_APP_TARBALL="$PWD/dist/AgentsSync.app.tar.gz" cargo build --release -p agents-sync

rm -rf dist/rel dist-npm
mkdir -p dist/rel dist-npm
cp target/release/agents-sync dist/rel/agents-sync
chmod 755 dist/rel/agents-sync
tar -czf "dist/rel/agents-sync-v$V-darwin-arm64.tar.gz" -C dist/rel agents-sync
( cd dist/rel && shasum -a 256 ./*.tar.gz > SHA256SUMS.txt && rm agents-sync )

cp -R npm/bin npm/lib npm/README.md dist-npm/
V="$V" node -e '
  const fs = require("fs");
  const p = JSON.parse(fs.readFileSync("npm/package.json", "utf8"));
  p.version = process.env.V;
  fs.writeFileSync("dist-npm/package.json", JSON.stringify(p, null, 2) + "\n");
'

echo "✓ dist/rel:"
ls -lh dist/rel
echo "✓ dist-npm:"
find dist-npm -type f | sort
