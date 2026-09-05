#!/usr/bin/env bash
# 讓一般使用者能開 /dev/uinput：裝 udev rule，把使用者加進 input 群組。
# 需要 root：sudo scripts/setup.sh
# 反向操作：sudo scripts/uninstall.sh
set -euo pipefail

RULE_FILE=/etc/udev/rules.d/80-wayhand-uinput.rules
GROUP=input

if [[ $EUID -ne 0 ]]; then
  echo "請用 sudo 執行：sudo $0" >&2
  exit 1
fi

TARGET_USER="${SUDO_USER:-}"
if [[ -z "$TARGET_USER" || "$TARGET_USER" == root ]]; then
  echo "找不到要設定的一般使用者（SUDO_USER 是空的或 root）。請從一般使用者帳號用 sudo 執行。" >&2
  exit 1
fi

if ! getent group "$GROUP" >/dev/null; then
  echo "系統沒有 $GROUP 群組，不繼續。" >&2
  exit 1
fi

has_wayhand_marker() {
  local first_line
  IFS= read -r first_line < "$1" &&
    [[ "$first_line" == \#*"wayhand-mcp"* ]]
}

if [[ -e "$RULE_FILE" || -L "$RULE_FILE" ]] && {
  [[ ! -f "$RULE_FILE" ]] || ! has_wayhand_marker "$RULE_FILE"
}; then
  echo "拒絕覆寫沒有 wayhand-mcp 標記的 udev rule：$RULE_FILE" >&2
  exit 1
fi

echo "[1/3] 寫入 udev rule：$RULE_FILE"
cat > "$RULE_FILE" <<RULE
# wayhand-mcp：讓 $GROUP 群組能讀寫 /dev/uinput，由 scripts/setup.sh 產生
KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="$GROUP", MODE="0660", OPTIONS+="static_node=uinput"
RULE

echo "[2/3] 把 $TARGET_USER 加進 $GROUP 群組"
if id -nG "$TARGET_USER" | tr ' ' '\n' | grep -qx "$GROUP"; then
  echo "      已經在群組裡，略過"
else
  usermod -aG "$GROUP" "$TARGET_USER"
fi

echo "[3/3] 重新載入 udev 並套用到 /dev/uinput"
udevadm control --reload
udevadm trigger --name-match=uinput
sleep 0.5
ls -l /dev/uinput

echo
echo "完成。群組變更要重新登入（登出再登入，或重開機）才生效。"
echo "登入後執行 scripts/check.sh 確認。"
