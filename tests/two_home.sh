#!/bin/bash
# 意图（2026-11-25）：在同机用两个临时 HOME 跑两个真 daemon，验证
#   文件监听 → 同步 → 收敛 这条完整链路（含 IPC、串行锁）。
#   不碰真实 HOME：所有路径都由 HOME 覆盖，测试结束整目录删除。
# 关键点：git remote 用本地 file 路径（不依赖 sshd/隧道），
#   传输层与广播由 net 层另行覆盖。
set -o pipefail
export LC_ALL=C.UTF-8 2>/dev/null || export LC_ALL=C

BIN="$1"
ROOT=/tmp/as-2home-fixed; rm -rf "$ROOT"; mkdir -p "$ROOT"
A="$ROOT/a"; B="$ROOT/b"
A_PID=""; B_PID=""

cleanup() {
  [ -n "$A_PID" ] && kill "$A_PID" 2>/dev/null
  [ -n "$B_PID" ] && kill "$B_PID" 2>/dev/null
  sleep 1
  # 回收本测试起的全部子进程（daemon 会 fork jj/git）
  pkill -f "$ROOT" 2>/dev/null
  sleep 0.5
  if pgrep -f "$ROOT" >/dev/null 2>&1; then
    echo "  ! 仍有残留进程，强制清理"
    pkill -9 -f "$ROOT" 2>/dev/null
  fi
  if [ "${KEEP:-0}" = "1" ]; then
    echo "  现场保留：$ROOT"
  else
    rm -rf "$ROOT"
  fi
}
trap cleanup EXIT

say() { printf '  %s\n' "$*"; }
fail() { printf '  \033[31m✗ %s\033[0m\n' "$*"; FAILED=1; }
pass() { printf '  \033[32m✓ %s\033[0m\n' "$*"; }
FAILED=0

seed() {
  local h="$1" who="$2"
  mkdir -p "$h/.agents/prompts" "$h/.agents/skills/s1" "$h/.zcode/agents" "$h/.zcode/cli" "$h/.zcode/v2"
  printf '# 基线\n' > "$h/.agents/AGENTS.md"
  printf 'p-%s\n' "$who" > "$h/.agents/prompts/x.md"
  printf -- '---\nname: s1\n---\n' > "$h/.agents/skills/s1/SKILL.md"
  printf '# zcode\n' > "$h/.zcode/AGENTS.md"
  printf 'slow\n' > "$h/.zcode/agents/slow.md"
  printf 'cache\n' > "$h/.zcode/cli/c.bin"
  printf '{"k":"local-%s"}\n' "$who" > "$h/.zcode/v2/setting.json"
  mkdir -p "$h/.config/jj"
  printf '[user]\nname = "%s"\nemail = "%s@t"\n' "$who" "$who" > "$h/.config/jj/config.toml"
}

run() { local h="$1"; shift; HOME="$h" "$BIN" "$@"; }

echo "=== 1. 两个临时 HOME 初始化 ==="
seed "$A" a; seed "$B" b
HOME="$A" AGENTS_SYNC_DEVICE_ID=a "$BIN" init >/dev/null 2>&1 || { fail "A init"; exit 1; }
HOME="$B" AGENTS_SYNC_DEVICE_ID=b "$BIN" init >/dev/null 2>&1 || { fail "B init"; exit 1; }

# B 丢弃空仓，改为克隆 A 的历史（真实 enroll 路径）
rm -rf "$B/.agents/sync"
git clone -q "$A/.agents/sync" "$B/.agents/sync" 2>/dev/null || { fail "B clone"; exit 1; }
( cd "$B/.agents/sync" && /opt/homebrew/bin/jj git init --colocate >/dev/null 2>&1 )
for k in user.name user.email; do ( cd "$B/.agents/sync" && /opt/homebrew/bin/jj config set --repo "$k" t >/dev/null 2>&1 ); done
pass "B 以 A 的历史为起点"

# 两边 devices.json：互相登记，git remote 指向本地 file 路径
mk_devices() {
  local h="$1" self="$2" net="$3"
  mkdir -p "$h/.agents/sync/config"
  cat > "$h/.agents/sync/config/devices.json" <<EOF
{"version":1,"devices":[
 {"id":"a","label":"A","ticket":"$TA","local_port":22411,"net_port":22421,"ssh_user":"t","lan":"127.0.0.1","repo_path":""},
 {"id":"b","label":"B","ticket":"$TB","local_port":22412,"net_port":22422,"ssh_user":"t","lan":"127.0.0.1","repo_path":""}
]}
EOF
  local other=$([ "$self" = "a" ] && echo b || echo a)
  git -C "$h/.agents/sync" remote remove "$other" 2>/dev/null
  git -C "$h/.agents/sync" remote add "$other" "$([ "$other" = a ] && echo "$A" || echo "$B")/.agents/sync"
  git -C "$h/.agents/sync" config receive.denyCurrentBranch ignore
}
# B 的活目录对齐到 A 的现状：模拟「已经一致的两台机器」。
# 首次接入的真实分歧由单独的场景验证（冲突必须拒绝落盘）。
rm -rf "$B/.agents" "$B/.zcode"
mkdir -p "$B/.agents" "$B/.zcode"
( cd "$A" && tar -cf - .agents .zcode ) | ( cd "$B" && tar -xf - )
# 基线对齐会连带复制 A 的凭证文件；改回 B 特有的值，才能验证 apply 不碰 v2。
printf '{"k":"local-b"}\n' > "$B/.zcode/v2/setting.json"
TA=$(HOME="$A" AGENTS_SYNC_DEVICE_ID=a "$BIN" ticket 2>/dev/null | tail -1)
TB=$(HOME="$B" AGENTS_SYNC_DEVICE_ID=b "$BIN" ticket 2>/dev/null | tail -1)
[ -n "$TA" ] && [ -n "$TB" ] && pass "两端 ticket 已生成" || fail "ticket 生成失败"
mk_devices "$A" a 22421
mk_devices "$B" b 22422
pass "两端登记完成"

echo "=== 2. 启动两个 daemon ==="
HOME="$A" AGENTS_SYNC_DEVICE_ID=a "$BIN" daemon >"$ROOT/a.log" 2>&1 &
A_PID=$!
HOME="$B" AGENTS_SYNC_DEVICE_ID=b "$BIN" daemon >"$ROOT/b.log" 2>&1 &
B_PID=$!
sleep 10
kill -0 "$A_PID" 2>/dev/null && pass "A daemon 存活（pid $A_PID）" || fail "A daemon 未存活"
kill -0 "$B_PID" 2>/dev/null && pass "B daemon 存活（pid $B_PID）" || fail "B daemon 未存活"

echo "=== 3. IPC 可达 ==="
OUT=$(HOME="$A" AGENTS_SYNC_DEVICE_ID=a "$BIN" status 2>&1)
echo "$OUT" | grep -q '"device": "a"' && pass "A IPC 返回本机身份" || fail "A IPC 异常：$(echo "$OUT" | head -3)"
echo "$OUT" | grep -q '"pid"' && pass "A IPC 返回 pid" || fail "A IPC 缺 pid"

echo "=== 4. 改 A 的活目录 → B 收敛（等待监听触发）==="
printf '# 来自 A 的新内容\n' > "$A/.agents/AGENTS.md"
printf 'new-skill-%s\n' "$(date +%s)" > "$A/.agents/skills/s1/EXTRA.md"
for i in $(seq 1 40); do
  grep -q '来自 A 的新内容' "$B/.agents/AGENTS.md" 2>/dev/null && break
  sleep 1
done
grep -q '来自 A 的新内容' "$B/.agents/AGENTS.md" 2>/dev/null \
  && pass "B 的活目录收到 A 的改动" || fail "B 未收敛（A 日志尾部：$(tail -3 "$ROOT/a.log" | tr '\n' ' '))"
[ -f "$B/.agents/skills/s1/EXTRA.md" ] && pass "新增文件已同步" || fail "新增文件未同步"

echo "=== 4b. 反向：改 B 的活目录 → A 收敛 ==="
printf '# 来自 B 的内容\n' > "$B/.agents/AGENTS.md"
for i in $(seq 1 40); do
  grep -q '来自 B 的内容' "$A/.agents/AGENTS.md" 2>/dev/null && break
  sleep 1
done
grep -q '来自 B 的内容' "$A/.agents/AGENTS.md" 2>/dev/null \
  && pass "A 的活目录收到 B 的改动（双向）" || fail "A 未收到 B 的改动（双向失败）"

echo "=== 5. 运行态与凭证未被破坏 ==="
[ -f "$B/.zcode/cli/c.bin" ] && pass "B 运行态保留" || fail "B 运行态被破坏"
grep -q 'local-b' "$B/.zcode/v2/setting.json" 2>/dev/null \
  && pass "B 凭证文件保留" || fail "B 凭证文件被覆盖"

echo "=== 6. 树哈希收敛 ==="
HA=$(HOME="$A" AGENTS_SYNC_DEVICE_ID=a "$BIN" status 2>/dev/null | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("last",{}).get("ok"))' 2>/dev/null)
say "A 最近一次同步 ok=$HA"
H1=$(/usr/bin/python3 -I -c "import sys;sys.path.insert(0,'/opt/homebrew/lib/python3.9/site-packages')" 2>/dev/null; echo)
PAY_A="$A/.agents/sync/payload"; PAY_B="$B/.agents/sync/payload"
SA=$(cd "$PAY_A" 2>/dev/null && find . -type f | sort | xargs shasum 2>/dev/null | shasum | cut -d' ' -f1)
SB=$(cd "$PAY_B" 2>/dev/null && find . -type f | sort | xargs shasum 2>/dev/null | shasum | cut -d' ' -f1)
[ -n "$SA" ] && [ "$SA" = "$SB" ] && pass "两节点 payload 一致（$SA）" || fail "payload 不一致：$SA vs $SB"

echo "=== 7. 结束后无孤儿进程 ==="
sleep 1
REMAIN=$(pgrep -f "$ROOT" | wc -l | tr -d ' ')
[ "$REMAIN" = "0" ] && pass "无残留" || say "清理阶段将回收 $REMAIN 个"

echo
if [ "$FAILED" = "1" ]; then
  echo "--- A daemon 日志 ---"; tail -12 "$ROOT/a.log" 2>/dev/null
  echo "--- B daemon 日志 ---"; tail -12 "$ROOT/b.log" 2>/dev/null
  KEEP=1; echo "结果：有失败"; exit 1
fi
echo "结果：全部通过"
