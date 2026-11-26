#!/usr/bin/env bash
# 并行分块下载（弱网专用）：N 个 Range 请求并发，每块独立重试，最后按序拼接。
#
#   scripts/parallel-download.sh <url> <out> [并发数=8] [块大小=2M]
#
# 说明：GitHub release 的 CDN 对单连接限速较狠，并行能成倍提速；
# 走直连（去掉代理环境变量）通常比走代理快数倍。
set -euo pipefail
URL="$1"
OUT="$2"
JOBS="${3:-8}"
CHUNK="${4:-2097152}"
DIR="$OUT.parts"
LOG="$OUT.log"

SIZE=$(env -u http_proxy -u https_proxy -u all_proxy curl -sS -L -I "$URL" \
  | awk 'tolower($1)=="content-length:"{n=$2} END{print n}' | tr -d '\r')
[ -n "$SIZE" ] && [ "$SIZE" -gt 0 ] || { echo "拿不到 content-length"; exit 1; }
N=$(( (SIZE + CHUNK - 1) / CHUNK ))
mkdir -p "$DIR"
echo "size=$SIZE chunk=$CHUNK parts=$N jobs=$JOBS" | tee "$LOG"

export URL CHUNK SIZE DIR
seq 0 $((N - 1)) | xargs -P "$JOBS" -I{} bash -c '
  i=$1
  start=$(( i * CHUNK ))
  end=$(( start + CHUNK - 1 ))
  [ "$end" -ge "$SIZE" ] && end=$(( SIZE - 1 ))
  want=$(( end - start + 1 ))
  out="$DIR/$i.bin"
  [ -f "$out" ] && [ "$(stat -f%z "$out")" -eq "$want" ] && exit 0
  for try in 1 2 3 4 5 6; do
    if env -u http_proxy -u https_proxy -u all_proxy \
        curl -sSL --http1.1 -m 300 -r "$start-$end" "$URL" -o "$out" \
       && [ "$(stat -f%z "$out")" -eq "$want" ]; then
      exit 0
    fi
    sleep $(( try * 2 ))
  done
  echo "chunk $i failed" >&2
  exit 1
' _ {}

: > "$OUT"
for i in $(seq 0 $((N - 1))); do
  cat "$DIR/$i.bin" >> "$OUT"
done
rm -rf "$DIR"
echo "DONE $(stat -f%z "$OUT")" | tee -a "$LOG"
