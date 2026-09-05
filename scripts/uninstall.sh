#!/usr/bin/env bash
# 還原 scripts/setup.sh 做的事：移除 udev rule，把使用者從 input 群組拿掉。
# 需要 root：sudo scripts/uninstall.sh
set -euo pipefail

RULE_FILE=/etc/udev/rules.d/80-wayhand-uinput.rules
GROUP=input

if [[ $EUID -ne 0 ]]; then
  echo "請用 sudo 執行：sudo $0" >&2
  exit 1
fi

TARGET_USER="${SUDO_USER:-}"

has_wayhand_marker() {
  local first_line
  IFS= read -r first_line < "$1" &&
    [[ "$first_line" == \#*"wayhand-mcp"* ]]
}

echo "[1/3] 移除 udev rule：$RULE_FILE"
if [[ -f "$RULE_FILE" ]]; then
  if has_wayhand_marker "$RULE_FILE"; then
    rm -f "$RULE_FILE"
  else
    echo "      找不到 wayhand-mcp 標記，保留：$RULE_FILE"
  fi
else
  echo "      不存在，略過"
fi

echo "[2/3] 把使用者從 $GROUP 群組移除"
if [[ -n "$TARGET_USER" && "$TARGET_USER" != root ]] && id -nG "$TARGET_USER" | tr ' ' '\n' | grep -qx "$GROUP"; then
  gpasswd -d "$TARGET_USER" "$GROUP"
else
  echo "      $TARGET_USER 不在群組裡或沒有指定使用者，略過"
fi

echo "[3/3] 重新載入 udev，讓 /dev/uinput 回到預設權限"
udevadm control --reload
udevadm trigger --name-match=uinput
sleep 0.5
ls -l /dev/uinput

echo
echo "完成。群組變更要重新登入才生效。"
