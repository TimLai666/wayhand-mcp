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

if (( $# == 1 )); then
  APP_ID=$1
else
  if ! APP_ID=$(detect_app_id); then
    echo "無法從 /proc/self/cgroup 推導目前程序的 app id，請明確傳入：$0 <app-id>" >&2
    exit 1
  fi
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

grant_permission ""
grant_permission "$APP_ID"

echo "PermissionStore Lookup ss screenshot screenshot："
busctl --user call \
  "$PERMISSION_STORE_SERVICE" \
  "$PERMISSION_STORE_PATH" \
  "$PERMISSION_STORE_INTERFACE" \
  Lookup ss screenshot screenshot
