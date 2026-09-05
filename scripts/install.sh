#!/usr/bin/env bash
# wayhand-mcp 一鍵安裝：下載 release、裝到 ~/.local、設定 /dev/uinput 權限、
# 預先授權截圖，並註冊到 Claude Code 與（或）Codex。
#
#   curl -fsSL https://raw.githubusercontent.com/TimLai666/wayhand-mcp/main/scripts/install.sh | bash
#
# 選項（放在 bash -s -- 之後，或直接執行腳本時給）：
#   --version vX.Y.Z   指定 release 版本（預設最新）
#   --local <dir>      不下載，改用本機 repo 目錄裡的 target/release/wayhand-mcp 與 scripts/
#   --claude / --codex 只註冊到其中一個；都不給就偵測到哪個 CLI 就註冊哪個
#   --skip-sudo        跳過需要 sudo 的步驟（apt 安裝 sway、udev 權限）
set -euo pipefail

REPO="TimLai666/wayhand-mcp"
VERSION=""
LOCAL_DIR=""
REGISTER_CLAUDE=""
REGISTER_CODEX=""
SKIP_SUDO=0

while (( $# )); do
  case "$1" in
    --version) VERSION=$2; shift 2 ;;
    --local) LOCAL_DIR=$2; shift 2 ;;
    --claude) REGISTER_CLAUDE=1; shift ;;
    --codex) REGISTER_CODEX=1; shift ;;
    --skip-sudo) SKIP_SUDO=1; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "未知參數：$1" >&2; exit 2 ;;
  esac
done
if [[ -z "$REGISTER_CLAUDE" && -z "$REGISTER_CODEX" ]]; then
  command -v claude >/dev/null 2>&1 && REGISTER_CLAUDE=1
  command -v codex >/dev/null 2>&1 && REGISTER_CODEX=1
fi

if [[ $EUID -eq 0 ]]; then
  echo "請用一般使用者執行，需要 root 的步驟會自己呼叫 sudo。" >&2
  exit 1
fi
if [[ "${XDG_SESSION_TYPE:-}" != wayland ]]; then
  echo "警告：目前不是 Wayland session（XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-未設定}），wayhand-mcp 只支援 GNOME Wayland。" >&2
fi
if [[ "$(uname -m)" != x86_64 ]]; then
  echo "目前只提供 x86_64 的 release，其他架構請從原始碼 cargo build --release。" >&2
  exit 1
fi

INSTALL_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/wayhand-mcp"
BIN_DIR="$HOME/.local/bin"
mkdir -p "$INSTALL_ROOT" "$BIN_DIR"

# 執行中的舊版不能直接覆蓋（text file busy），先寫暫存檔再原子替換；
# 舊程序繼續用舊的 inode，新 session 才會用新版。
install_binary() {
  cp "$1" "$INSTALL_ROOT/wayhand-mcp.new"
  chmod +x "$INSTALL_ROOT/wayhand-mcp.new"
  mv -f "$INSTALL_ROOT/wayhand-mcp.new" "$INSTALL_ROOT/wayhand-mcp"
}

echo "[1/5] 取得 wayhand-mcp"
if [[ -n "$LOCAL_DIR" ]]; then
  [[ -x "$LOCAL_DIR/target/release/wayhand-mcp" ]] || { echo "找不到 $LOCAL_DIR/target/release/wayhand-mcp，先 cargo build --release" >&2; exit 1; }
  install_binary "$LOCAL_DIR/target/release/wayhand-mcp"
  rm -rf "$INSTALL_ROOT/scripts" && cp -r "$LOCAL_DIR/scripts" "$INSTALL_ROOT/scripts"
else
  command -v curl >/dev/null || { echo "需要 curl" >&2; exit 1; }
  if [[ -z "$VERSION" ]]; then
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
    [[ -n "$VERSION" ]] || { echo "查不到最新 release 版本" >&2; exit 1; }
  fi
  ASSET="wayhand-mcp-${VERSION}-x86_64-linux.tar.gz"
  URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  echo "      下載 $URL"
  curl -fsSL -o "$TMP/$ASSET" "$URL"
  curl -fsSL -o "$TMP/$ASSET.sha256" "$URL.sha256"
  (cd "$TMP" && sha256sum -c "$ASSET.sha256" --quiet)
  tar -xzf "$TMP/$ASSET" -C "$TMP"
  install_binary "$TMP/wayhand-mcp"
  rm -rf "$INSTALL_ROOT/scripts" && cp -r "$TMP/scripts" "$INSTALL_ROOT/scripts"
fi
chmod +x "$INSTALL_ROOT"/scripts/*.sh
ln -sf "$INSTALL_ROOT/wayhand-mcp" "$BIN_DIR/wayhand-mcp"
echo "      安裝到 $INSTALL_ROOT，指令連結 $BIN_DIR/wayhand-mcp"

echo "[2/5] 系統套件（sway 給沙盒模式用、wl-clipboard 給中文貼上用）"
MISSING=()
command -v sway >/dev/null || MISSING+=(sway)
command -v wl-copy >/dev/null || MISSING+=(wl-clipboard)
if (( ${#MISSING[@]} )); then
  if (( SKIP_SUDO )); then
    echo "      跳過（--skip-sudo）。之後請自行：sudo apt install ${MISSING[*]}"
  else
    sudo apt-get install -y "${MISSING[@]}"
  fi
else
  echo "      已安裝"
fi

echo "[3/5] /dev/uinput 權限（真實桌面模式用）"
if [[ -w /dev/uinput ]]; then
  echo "      已可寫，略過"
elif (( SKIP_SUDO )); then
  echo "      跳過（--skip-sudo）。之後請自行：sudo $INSTALL_ROOT/scripts/setup.sh"
else
  sudo "$INSTALL_ROOT/scripts/setup.sh"
fi

echo "[4/5] 預先授權桌面截圖"
"$INSTALL_ROOT/scripts/grant-screenshot.sh" >/dev/null && echo "      完成" || echo "      失敗（沒有 portal 授權儲存區？），第一次截圖會跳一次授權視窗"

echo "[5/5] 註冊 MCP server"
if [[ -n "$REGISTER_CLAUDE" ]]; then
  if command -v claude >/dev/null; then
    claude mcp remove wayhand-mcp --scope user >/dev/null 2>&1 || true
    claude mcp add wayhand-mcp --scope user -- "$INSTALL_ROOT/wayhand-mcp"
  else
    echo "      找不到 claude 指令，略過 Claude Code"
  fi
fi
if [[ -n "$REGISTER_CODEX" ]]; then
  if command -v codex >/dev/null; then
    codex mcp remove wayhand-mcp >/dev/null 2>&1 || true
    codex mcp add wayhand-mcp -- "$INSTALL_ROOT/wayhand-mcp"
  else
    echo "      找不到 codex 指令，略過 Codex"
  fi
fi

echo
echo "完成。新開一個 Claude Code 或 Codex session 就會看到 wayhand-mcp 的工具。"
echo "自我檢查：$INSTALL_ROOT/scripts/check.sh"
