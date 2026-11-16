#!/usr/bin/env bash
# 用法: measure-cpu.sh <webview-pid> [秒数]  —— 打印该 Electron 进程树里各角色的 CPU 占用
set -euo pipefail
ROOT=${1:?webview pid}
SECS=${2:-10}
tree() { echo "$ROOT"; pgrep -P "$ROOT" 2>/dev/null | while read -r p; do tree "$p"; done; }
pids=$(tree | sort -u)
declare -A a b
for p in $pids; do a[$p]=$(awk '{print $14+$15}' /proc/$p/stat 2>/dev/null || echo 0); done
sleep "$SECS"
for p in $pids; do
  b[$p]=$(awk '{print $14+$15}' /proc/$p/stat 2>/dev/null || echo 0)
  name=$(tr '\0' ' ' < /proc/$p/cmdline 2>/dev/null | grep -o -- '--type=[a-z-]*' | head -1)
  [[ -z "$name" ]] && name=browser
  rss=$(( $(awk '/VmRSS/{print $2}' /proc/$p/status 2>/dev/null || echo 0) / 1024 ))
  printf '%-14s pid=%-7s cpu=%5.1f%%  rss=%4dMB\n' "$name" "$p" "$(echo "scale=1; (${b[$p]}-${a[$p]})*100/$SECS" | bc)" "$rss"
done
