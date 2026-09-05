#!/usr/bin/env bash
# 預先授權 XDG screenshot portal，避免首次截圖時出現授權對話框。
set -euo pipefail

PERMISSION_STORE_SERVICE=org.freedesktop.impl.portal.PermissionStore
PERMISSION_STORE_PATH=/org/freedesktop/impl/portal/PermissionStore
PERMISSION_STORE_INTERFACE=org.freedesktop.impl.portal.PermissionStore

if (( $# > 1 )); then
  echo "用法：$0 [app-id]" >&2
  exit 2
fi

if ! command -v busctl >/dev/null 2>&1; then
  echo "找不到 busctl，無法寫入 XDG screenshot portal 權限。" >&2
  exit 1
fi

detect_app_id() {
  local line last_line= cgroup_path scope app_id

  [[ -r /proc/self/cgroup ]] || return 1

  while IFS= read -r line; do
    last_line=$line
  done < /proc/self/cgroup

  cgroup_path=${last_line##*:}
  scope=${cgroup_path##*/}

  if [[ "$scope" =~ ^app-([^-]+-)?(.+)-[[:digit:]]+\.scope$ ]]; then
    app_id=${BASH_REMATCH[2]}
    app_id=${app_id//\\x2d/-}
    [[ -n "$app_id" ]] || return 1
    printf '%s\n' "$app_id"
    return 0
  fi

  return 1
}

# 一定授權的 app id：空字串（終端機底下啟動的 claude、codex 都以 host app 身分呼叫 portal），
# 以及 Claude 桌面版。從 app-*.scope 推得出目前程序的 app id 時也一併授權。
APP_IDS=("" "com.anthropic.Claude")
if (( $# == 1 )); then
  APP_IDS+=("$1")
elif APP_ID=$(detect_app_id); then
  APP_IDS+=("$APP_ID")
else
  echo "目前程序不在 app-*.scope 底下（例如從終端機執行），portal 會把它當成沒有 app id 的 host app，直接授權空 app id。"
fi

grant_permission() {
  local app_id=$1
  local label=${app_id:-<empty>}

  printf 'Granting screenshot permission for app id: %s\n' "$label"
  # SetPermission 的 signature 是 sbssas：table、create、id、app、permissions。
  busctl --user call \
    "$PERMISSION_STORE_SERVICE" \
    "$PERMISSION_STORE_PATH" \
    "$PERMISSION_STORE_INTERFACE" \
    SetPermission sbssas screenshot true screenshot "$app_id" 1 yes
}

GRANTED=()
for app_id in "${APP_IDS[@]}"; do
  duplicate=0
  for done_id in "${GRANTED[@]}"; do
    [[ "$done_id" == "$app_id" ]] && duplicate=1
  done
  (( duplicate )) && continue
  GRANTED+=("$app_id")
  grant_permission "$app_id"
done

echo "PermissionStore Lookup ss screenshot screenshot："
busctl --user call \
  "$PERMISSION_STORE_SERVICE" \
  "$PERMISSION_STORE_PATH" \
  "$PERMISSION_STORE_INTERFACE" \
  Lookup ss screenshot screenshot
