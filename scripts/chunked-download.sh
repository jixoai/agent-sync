#!/usr/bin/env bash
# 分块下载 GitHub Release 资产（弱网下可续传、可重试），用于 actions runner。
set -euo pipefail
URL="$1"
OUT="$2"
CHUNK=$((4 * 1024 * 1024))
LOG="$OUT.log"

SIZE=$(curl -sS -L -I "$URL" | awk 'tolower($1)=="content-length:"{n=$2} END{print n}' | tr -d '\r')
echo "total=$SIZE chunk=$CHUNK" | tee "$LOG"
: > "$OUT"
start=0
while [ "$start" -lt "$SIZE" ]; do
  end=$((start + CHUNK - 1))
  [ "$end" -ge "$SIZE" ] && end=$((SIZE - 1))
  want=$((end - start + 1))
  ok=0
  for try in $(seq 1 8); do
    if curl -sSL --http1.1 --retry 5 --retry-all-errors -m 180 \
        -r "$start-$end" "$URL" -o "$OUT.part" 2>>"$LOG" \
       && [ "$(stat -f%z "$OUT.part")" -eq "$want" ]; then
      ok=1; break
    fi
    echo "chunk $start-$end try $try failed" >> "$LOG"
    sleep 2
  done
  [ "$ok" = 1 ] || { echo "GIVEUP at $start" | tee -a "$LOG"; exit 1; }
  cat "$OUT.part" >> "$OUT"
  rm -f "$OUT.part"
  start=$((end + 1))
  echo "progress $start/$SIZE" >> "$LOG"
done
echo "DONE $OUT $(stat -f%z "$OUT")" | tee -a "$LOG"
