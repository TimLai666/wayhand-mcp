#!/usr/bin/env bash
# 不需要 root。確認目前這個登入 session 能不能開 /dev/uinput。
set -uo pipefail

ok=0
echo "session 類型：${XDG_SESSION_TYPE:-未知}"
[[ "${XDG_SESSION_TYPE:-}" == wayland ]] || { echo "  警告：不是 Wayland session"; ok=1; }

echo "目前群組：$(id -nG)"
if id -nG | tr ' ' '\n' | grep -qx input; then
  echo "  input 群組：有"
else
  echo "  input 群組：沒有。跑過 sudo scripts/setup.sh 之後要重新登入。"; ok=1
fi

echo "/dev/uinput：$(ls -l /dev/uinput 2>&1)"
if [[ -w /dev/uinput ]]; then
  echo "  可寫：是"
else
  echo "  可寫：否"; ok=1
fi

if [[ $ok -eq 0 ]]; then echo; echo "全部通過，可以注入輸入。"; else echo; echo "還沒就緒。"; fi
exit $ok
