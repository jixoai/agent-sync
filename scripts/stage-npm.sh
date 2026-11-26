#!/usr/bin/env bash
# 本地组装 npm 包（与 CI 同一套步骤），用于试跑发布/自更新，不联网。
#
#   scripts/stage-npm.sh [版本号]
#
# 产物：dist-npm/（package.json + bin/agents-sync + lib/AgentsSync.app）
set -euo pipefail
cd "$(dirname "$0")/.."

V="${1:-$(node -p "require('./npm/package.json').version" 2>/dev/null || echo 0.1.0)}"
echo "→ 版本 $V"

cargo build --release -p agents-sync
( cd app && nsh build && nsh package --target macos )

rm -rf dist-npm
mkdir -p dist-npm/bin dist-npm/lib
cp target/release/agents-sync dist-npm/bin/agents-sync
chmod 755 dist-npm/bin/agents-sync
cp -R app/.nsh/out/AgentsSync.app dist-npm/lib/
cp npm/README.md dist-npm/README.md

V="$V" node -e '
  const fs = require("fs");
  const p = JSON.parse(fs.readFileSync("npm/package.json", "utf8"));
  p.version = process.env.V;
  fs.writeFileSync("dist-npm/package.json", JSON.stringify(p, null, 2) + "\n");
'

echo "✓ dist-npm/ 就绪（$(du -sh dist-npm | cut -f1)）"
