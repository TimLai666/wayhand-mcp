#!/usr/bin/env bash
# 讓一般使用者能開 /dev/uinput：裝 udev rule，把使用者加進 input 群組。
# 需要 root：sudo scripts/setup.sh
# 反向操作：sudo scripts/uninstall.sh
set -euo pipefail

RULE_FILE=/etc/udev/rules.d/60-wayhand-uinput.rules
LEGACY_RULE_FILE=/etc/udev/rules.d/80-wayhand-uinput.rules
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

# 舊版本把 rule 放在 80，排在 systemd 設 uaccess ACL 的 73-seat-late.rules 之後，標籤來不及生效。
if [[ -f "$LEGACY_RULE_FILE" ]] && has_wayhand_marker "$LEGACY_RULE_FILE"; then
  echo "移除舊位置的 rule：$LEGACY_RULE_FILE"
  rm -f "$LEGACY_RULE_FILE"
fi

echo "[1/3] 寫入 udev rule：$RULE_FILE（必須排在 73-seat-late.rules 之前，uaccess 才會生效）"
cat > "$RULE_FILE" <<RULE
# wayhand-mcp：讓目前登入的使用者（uaccess ACL）與 $GROUP 群組能讀寫 /dev/uinput，由 scripts/setup.sh 產生
KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="$GROUP", MODE="0660", TAG+="uaccess", OPTIONS+="static_node=uinput"
RULE

echo "[2/3] 把 $TARGET_USER 加進 $GROUP 群組"
if id -nG "$TARGET_USER" | tr ' ' '\n' | grep -qx "$GROUP"; then
  echo "      已經在群組裡，略過"
else
  usermod -aG "$GROUP" "$TARGET_USER"
fi

echo "[3/3] 重新載入 udev 並套用到 /dev/uinput"
udevadm control --reload
udevadm trigger --action=add --name-match=uinput
sleep 0.5
ls -l /dev/uinput
getfacl -p /dev/uinput 2>/dev/null | grep '^user:' || true

echo
echo "完成。目前登入的使用者透過 ACL 立刻可用，不必重新登入。"
echo "以一般使用者執行 scripts/check.sh 確認。"
