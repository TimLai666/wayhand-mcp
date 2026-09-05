# AGENTS.md — wayhand-mcp

本檔是這個專案的操作規範。動手前先讀完。

## 專案目標

做一個本機 MCP server，讓 Claude Code 在 GNOME Wayland 上完成「截圖 → 分析 → 點擊／拖曳／打字 → 再截圖驗證」的完整迴圈。
只支援 Wayland。不要用 xdotool、pyautogui 這類只在 X11 能用的做法。

## 現況（2026-09-04）

交付順序四步都完成。沙盒 demo 與真實桌面 demo（以沙盒視窗當受測程式）都在真機通過，截圖在 `docs/demo/`。`calibrate` 用沙盒視窗當量尺，實測偏差 0.00 px。

已查證的環境：

| 項目 | 狀態 |
|---|---|
| 桌面 | Zorin OS 18.1，GNOME Shell 46.0，`XDG_SESSION_TYPE=wayland` |
| Rust | cargo 1.97.1（`~/.cargo/bin`） |
| rmcp crate | crates.io 最新 3.2.0（`rmcp`、`rmcp-macros`），加依賴前用 `cargo info rmcp` 再確認 API |
| ydotool | 未安裝。apt 的 0.1.8（2020 年）`mousemove` 只有相對移動、沒有 `--absolute`、沒有 `--socket-path`，也不附 systemd unit，不能用。上游最新 v1.0.4 才有這些。 |
| `/dev/uinput` | 核心內建（`CONFIG_INPUT_UINPUT=y`），預設 `crw------- root root`。權限由 `scripts/setup.sh` 處理（udev rule 帶 `uaccess` 標籤，logind 設 ACL；input 群組備用），需要 sudo，不用重新登入。 |
| 其他 | `gnome-text-editor`、`wl-copy`、`wl-paste` 都在 `/usr/bin` |

## 技術選型

- 語言 Rust，MCP 用官方 `rmcp` SDK，只走 stdio transport，不開任何網路埠。
- 真實桌面模式的輸入注入由 server 直接開 `/dev/uinput` 建一個長駐虛擬裝置（evdev crate），不用 ydotool。
- 截圖優先用 XDG desktop portal 的 `org.freedesktop.portal.Screenshot`，以 zbus 呼叫。
  GNOME 可能每次都跳授權視窗。實測後若無法免互動連續截圖，改評估 ScreenCast portal 的持續授權 session 或 gnome-remote-desktop，取捨要寫進 README 再決定。

## 兩種操作模式（2026-09-04 決定）

GNOME 的 Mutter 只有一組游標與鍵盤焦點，注入的輸入一定會搶走使用者的游標，也無法送給背景視窗。所以提供兩種模式，由每個工具的 `target` 參數選擇，工具 description 要推薦 `sandbox`：

| 模式 | `target` 值 | 做法 | 影響使用者 |
|---|---|---|---|
| 沙盒桌面（預設、推薦） | `sandbox` | server 啟動一個私有的 sway（wlroots），預設 headless 完全不顯示，`visible: true` 才以視窗呈現。要操作的程式在裡面啟動。輸入走 `zwlr_virtual_pointer_v1` 與 `zwp_virtual_keyboard_v1`，截圖走 `zwlr_screencopy_v1`，都是對巢狀 compositor 講話 | 不影響，使用者可以繼續用電腦，該視窗可放背景或別的工作區 |
| 真實桌面 | `desktop` | 現有做法：uinput 注入、XDG Screenshot portal 截圖 | 搶走真實游標與鍵盤，執行時人不能碰電腦 |

沙盒模式的座標：截圖像素直接對應虛擬指標的絕對座標（`motion_absolute` 帶 x_extent/y_extent 就是截圖寬高），不需要校準。`calibrate` 只有真實桌面模式需要。

沙盒的生命週期由工具管理：`sandbox_start`（headless 預設 1920×1080 可指定；visible 模式大小由 GNOME 決定，實測 1280×720）、`sandbox_launch`（在沙盒內啟動一個程式，參數是 argv 陣列，不經 shell）、`sandbox_stop`。server 結束時要把巢狀 compositor 一起收掉。

## MCP 工具清單

`screen_info`、`screenshot`、`click`、`double_click`、`right_click`、`move`、`drag`（支援中途多個路徑點）、`scroll`、`type`、`key`（組合鍵，例如 `ctrl+shift+t`）、`calibrate`，以及沙盒管理的 `sandbox_start`、`sandbox_launch`、`sandbox_stop`。除了 `calibrate` 與沙盒管理工具，其餘都有 `target` 參數（`sandbox` 預設、`desktop`）。

每個工具的 description 必須寫清楚座標系與單位。`screenshot` 回傳 PNG image content 並附時間戳。`type` 要處理中文輸入的限制並誠實記錄做不到的部分。

## 必須解決的正確性問題

1. 座標對映：截圖像素座標、HiDPI 縮放倍率、ydotool 絕對座標三者的換算要實測校準。`calibrate` 流程是注入移動 → 截圖找游標 → 算轉換矩陣。不要假設 1:1。驗收誤差小於 3px。
2. 時序：點擊後 UI 有動畫，工具要有可選的 settle 延遲。連續操作之間要節流。
3. 授權：`/dev/uinput` 權限用 udev rule 加群組解決，裝完後一般使用者可用。server 不得以 root 執行。

## 安全規則

- server 只綁 stdio 給本機 Claude 用，不開網路埠。
- `type` 與 `key` 純注入，不得執行任何 shell。
- README 明寫：操作的是使用者真實的游標與鍵盤，執行時人不要碰電腦。
- 緊急停止：收到 SIGINT，或連續注入次數超過上限，就熔斷停止注入。

## 前置設定（需要 root）

每一步都做成 setup 腳本，並附可逆的 uninstall：寫 `/dev/uinput` 的 udev rule、把使用者加進 input 群組。沙盒模式另外需要 `apt install sway`。
任何需要 sudo 的步驟都要先徵求使用者同意，不要自己跑。

## 驗收標準

- `claude mcp add wayhand-mcp -- <指令>` 註冊後，新 session 看得到全部工具。
- 端對端 demo 全程由 MCP 工具完成並附截圖：開啟 GNOME 文字編輯器 → 截圖 → 點進輸入區 → 打字 → 截圖驗證文字出現 → 拖曳選取 → `ctrl+c` → 用 `wl-paste` 驗證剪貼簿。
- 座標校準誤差小於 3px，截圖到可操作的往返延遲要有實測數字。
- 單元測試涵蓋座標換算與參數驗證。注入類功能用假 backend 測，真注入只在 demo 跑。
- 做不到的事（觸控板多指手勢、中文 IME 輸入限制等）寫進 README，不要略過。

## 交付順序

小步交付，每步驗證後回報：

1. setup 腳本 + `screenshot` + `click`，跑通最小版 demo。
2. 補齊其餘工具。
3. `calibrate` 與校準測試。
4. 完整端對端 demo 與 README。

## 指令

專案採 Cargo 後，以下是標準用法：

```bash
cargo build
cargo test
cargo test <測試名稱>        # 跑單一測試
cargo clippy --all-targets
cargo run --bin wayhand-mcp  # 以 stdio 啟動 server（給 claude mcp add 用）
```

## 必讀文件

- `ENG.md`：架構、測試接縫、已量測數字與尚未驗證的假設。改架構、選測試接縫或動座標對映之前先讀。
- `scripts/check.sh`：確認目前登入 session 能不能開 `/dev/uinput`，跑真注入前先跑。

## 架構重點

- 注入層是 `Injector` trait：uinput、沙盒（Wayland 虛擬輸入）、假 backend 三個實作，工具層只依賴 trait。工具動作先由純函式產生 `Step` 序列再執行，序列本身可單元測試。
- 座標換算獨立成純函式模組，校準矩陣由 `calibrate` 產生並持久化，所有座標工具都經過它。
- 截圖層與注入層分開，截圖失敗不影響注入熔斷狀態。

## Follow-ups

- uinput 權限用 udev `TAG+="uaccess"`（rule 檔編號必須小於 73 才會生效）由 logind 設 ACL，不必重新登入；input 群組只是備用。
- uinput 指標裝置只能宣告三個滑鼠鍵，宣告 BTN_TOOL_PEN/BTN_TOUCH 會被 udev 判成繪圖板而讓 Mutter 忽略絕對移動，所以鍵盤是另一個裝置。
- GNOME portal 截圖不含游標，`calibrate` 改用沙盒視窗（純洋紅背景）當量尺。
- MCP 取消訊號（request cancellation）沒有傳進工具，取消後進行中的動作會做完。
- 截圖與座標沒有綁定識別碼，只靠操作佇列序列化。單一呼叫者下沒問題。
- visible 沙盒視窗被遮住、縮小或在別的工作區時 GNOME 不給畫面，screencopy 會 5 秒逾時；headless 沒這問題，所以 headless 是預設。`calibrate` 自己開臨時的可見量尺視窗（tag `ruler`），不動工作用的沙盒。
