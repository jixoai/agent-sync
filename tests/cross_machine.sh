#!/bin/bash
# 意图（2026-11-25）：真跨机验收。mini ↔ iMac，各自用临时 HOME，绝不碰真实配置。
#   验证：iroh 直连打洞 → 双向可达 → 监听触发 → 同步 → 收敛。
# 与双 HOME 测试的唯一区别：传输真的跨了机器（同一份二进制、各自的 sshd）。
set -o pipefail
export LC_ALL=C

BIN="$1"
IMAC=kzf@192.168.1.106
MINI_LAN=$(ipconfig getifaddr en0 2>/dev/null); [ -n "$MINI_LAN" ] || MINI_LAN=192.168.1.115
IMAC_LAN=192.168.1.106
RA=/tmp/as-x            # mini 侧根
RB=/tmp/as-x            # iMac 侧根
A="$RA/a"; B="$RB/b"
FAILED=0
say()  { printf '  %s\n' "$*"; }
pass() { printf '  \033[32m✓ %s\033[0m\n' "$*"; }
fail() { printf '  \033[31m✗ %s\033[0m\n' "$*"; FAILED=1; }

I() { ssh -o BatchMode=yes -o ConnectTimeout=8 "$IMAC" "$@"; }

cleanup() {
  pkill -f "agents-sync daemon" 2>/dev/null
  I "pkill -f 'agents-sync daemon' 2>/dev/null; rm -rf '$RB'" >/dev/null 2>&1
  sleep 1
  pkill -9 -f "agents-sync daemon" 2>/dev/null
  I "pkill -9 -f 'agents-sync daemon' 2>/dev/null" >/dev/null 2>&1
  rm -rf "$RA" 2>/dev/null
}
trap cleanup EXIT

seed() {
  local h="$1"
  mkdir -p "$h/.agents/prompts" "$h/.agents/skills/s1" "$h/.zcode/agents" "$h/.zcode/cli" "$h/.zcode/v2" "$h/.config/jj" "$h/.ssh"
  printf '# 基线\n' > "$h/.agents/AGENTS.md"
  printf 'p\n' > "$h/.agents/prompts/x.md"
  printf -- '---\nname: s1\n---\n' > "$h/.agents/skills/s1/SKILL.md"
  printf '# zcode\n' > "$h/.zcode/AGENTS.md"
  printf 'slow\n' > "$h/.zcode/agents/slow.md"
  printf 'cache\n' > "$h/.zcode/cli/c.bin"
  printf '{"k":"local"}\n' > "$h/.zcode/v2/setting.json"
  printf '[user]\nname = "t"\nemail = "t@t"\n' > "$h/.config/jj/config.toml"
  # 临时 HOME 里也要能 ssh：指向真实的 key
  printf 'Host *\n  IdentityFile ~/.ssh/id_ed25519\n  IdentityFile /Users/kzf/.ssh/id_ed25519\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n' > "$h/.ssh/config"
  chmod 600 "$h/.ssh/config"
}

echo "=== 1. 两端临时 HOME ==="
rm -rf "$RA"; mkdir -p "$RA"
I "rm -rf '$RB'; mkdir -p '$RB'"
seed "$A"
# 把 seed 函数送到 iMac 执行
I "bash -s" <<EOF
$(declare -f seed)
seed "$B"
EOF
[ -f "$A/.agents/AGENTS.md" ] && pass "mini 侧就绪" || fail "mini 侧失败"
I "[ -f '$B/.agents/AGENTS.md' ]" && pass "iMac 侧就绪" || fail "iMac 侧失败"

echo "=== 2. 各自 init 并取 ticket ==="
HOME="$A" AGENTS_SYNC_DEVICE_ID=a "$BIN" init >/dev/null 2>&1 || fail "mini init"
I "HOME='$B' AGENTS_SYNC_DEVICE_ID=b /tmp/agents-sync init >/dev/null 2>&1" || fail "iMac init"
# ticket 必须在「传仓」之后取：传仓会重建对端的数据仓目录，
# 若先取 ticket 再覆盖，对端的 .secrets 会被抹掉，daemon 启动时另行生成新身份，
# 于是配置里的 ticket 与实际证书不匹配（表现为 UnknownIssuer）。

echo "=== 3. B 以 A 的历史为起点（跨机传仓）==="
tar -czf /tmp/as-a-repo.tgz -C "$A/.agents" --exclude='sync/.jj' --exclude='sync/logs' --exclude='sync/locks' --exclude='sync/.secrets' --exclude='sync/.state' sync
scp -q /tmp/as-a-repo.tgz "$IMAC:/tmp/as-a-repo.tgz"
I "rm -rf '$B/.agents/sync' && mkdir -p '$B/.agents' && tar -xzf /tmp/as-a-repo.tgz -C '$B/.agents' && cd '$B/.agents/sync' && /opt/homebrew/bin/jj git init --colocate >/dev/null 2>&1 && /opt/homebrew/bin/jj config set --repo user.name t >/dev/null 2>&1 && /opt/homebrew/bin/jj config set --repo user.email t@t >/dev/null 2>&1" \
  && pass "iMac 侧建立共享历史" || fail "iMac 侧建仓失败"
rm -f /tmp/as-a-repo.tgz

echo "=== 4. 登记对端 ==="
TA=$(HOME="$A" AGENTS_SYNC_DEVICE_ID=a "$BIN" ticket 2>/dev/null | tail -1)
TB=$(I "HOME='$B' AGENTS_SYNC_DEVICE_ID=b /tmp/agents-sync ticket 2>/dev/null" | tail -1)
say "mini ticket: ${TA:0:24}…"
say "iMac ticket: ${TB:0:24}…"
[ -n "$TA" ] && [ -n "$TB" ] && pass "ticket 就绪（传仓之后生成）" || fail "ticket 缺失"

# 取各自 relay 地址：受限网络下纯出站连通靠它
RA=$(HOME="$A" AGENTS_SYNC_DEVICE_ID=a "$BIN" net 2>/dev/null | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["self"]["relay_url"])' 2>/dev/null)
RB=$(I "HOME='$B' AGENTS_SYNC_DEVICE_ID=b /tmp/agents-sync net 2>/dev/null" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin)["self"]["relay_url"])' 2>/dev/null)
say "mini relay: ${RA:-<none>}"
say "iMac relay: ${RB:-<none>}"

devices() {
  cat <<JSON
{"version":1,"devices":[
 {"id":"a","label":"mini","ticket":"$TA","local_port":22411,"net_port":22421,"ssh_user":"kzf","lan":"$MINI_LAN","repo_path":"$A/.agents/sync","relay_url":"$RA"},
 {"id":"b","label":"imac","ticket":"$TB","local_port":22412,"net_port":22422,"ssh_user":"kzf","lan":"$IMAC_LAN","repo_path":"$B/.agents/sync","relay_url":"$RB"}
]}
JSON
}
mkdir -p "$A/.agents/sync/config"
devices > "$A/.agents/sync/config/devices.json"
devices > /tmp/as-devices-b.json
scp -q /tmp/as-devices-b.json "$IMAC:$B/.agents/sync/config/devices.json"
rm -f /tmp/as-devices-b.json
pass "两端注册表就绪（直连地址优先）"

echo "=== 5. 启动 daemon ==="
HOME="$A" AGENTS_SYNC_DEVICE_ID=a "$BIN" daemon >/tmp/as-a.log 2>&1 &
A_PID=$!
I "cd /tmp && HOME='$B' AGENTS_SYNC_DEVICE_ID=b nohup /tmp/agents-sync daemon >/tmp/as-b.log 2>&1 </dev/null & echo started" >/dev/null
sleep 12
kill -0 "$A_PID" 2>/dev/null && pass "mini daemon 存活" || fail "mini daemon 退出"
I "pgrep -f '/tmp/agents-sync daemon' >/dev/null" && pass "iMac daemon 存活" || fail "iMac daemon 未起"

echo "=== 5b. 广播连通性（daemon 起来之后再测）==="
PING_AB=$(HOME="$A" AGENTS_SYNC_DEVICE_ID=a timeout 25 "$BIN" ping b 2>&1 | grep -E "^ping b"); echo "     $PING_AB"
if echo "$PING_AB" | grep -q "ping b ok"; then
  pass "mini → iMac 广播可达"
else
  fail "mini → iMac 广播不可达"
fi
PING_BA=$(I "HOME='$B' AGENTS_SYNC_DEVICE_ID=b timeout 25 /tmp/agents-sync ping a 2>&1 | grep -E '^ping a'"); echo "     $PING_BA"
if echo "$PING_BA" | grep -q "ping a ok"; then
  pass "iMac → mini 广播可达"
else
  fail "iMac → mini 广播不可达"
fi

echo "=== 5c. SSH 穿过隧道（TCP 代理 + 真 sshd）==="
SSH_TUN=$(HOME="$A" timeout 20 ssh -o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=no \
  -o UserKnownHostsFile=/dev/null -p 22412 kzf@127.0.0.1 'echo SSH_TUNNEL_OK' 2>&1 | tail -1)
if echo "$SSH_TUN" | grep -q SSH_TUNNEL_OK; then
  pass "mini→iMac 的 SSH 隧道可用"
else
  fail "mini→iMac 的 SSH 隧道不可用：$SSH_TUN"
fi

# 诊断模式：只跑到这一步就结束（省时间定位单点问题）
if [ "${STOP_AFTER_TUNNEL:-0}" = "1" ]; then echo "结果：诊断模式结束"; exit 0; fi

echo "=== 6. mini 改一处 → iMac 收敛 ==="
printf '# 来自 mini 的跨机改动\n' > "$A/.agents/AGENTS.md"
OK=0
for i in $(seq 1 60); do
  if I "grep -q '来自 mini 的跨机改动' '$B/.agents/AGENTS.md' 2>/dev/null"; then OK=1; break; fi
  sleep 1
done
[ "$OK" = "1" ] && pass "iMac 的活目录收到 mini 的改动" || fail "iMac 未收敛"

echo "=== 7. 反向：iMac 改 → mini 收敛 ==="
I "printf '# 来自 iMac 的改动\n' > '$B/.agents/AGENTS.md'"
OK=0
for i in $(seq 1 60); do
  if grep -q '来自 iMac 的改动' "$A/.agents/AGENTS.md" 2>/dev/null; then OK=1; break; fi
  sleep 1
done
[ "$OK" = "1" ] && pass "mini 收到 iMac 的改动（双向通）" || fail "mini 未收敛"

echo "=== 8. 运行态未被破坏 ==="
I "[ -f '$B/.zcode/cli/c.bin' ]" && pass "iMac 运行态保留" || fail "iMac 运行态被破坏"

echo
if [ "$FAILED" = "1" ]; then
  echo "--- mini daemon 日志 ---"; tail -8 /tmp/as-a.log 2>/dev/null
  echo "--- iMac daemon 日志 ---"; I "tail -8 /tmp/as-b.log 2>/dev/null"
  echo "结果：有失败"; exit 1
fi
echo "结果：全部通过"
