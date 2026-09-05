# wayhand-mcp

`wayhand-mcp` is a local MCP server that lets Claude Code see and operate a
GNOME Wayland desktop: screenshot, click/type/drag by screenshot pixel
coordinates, screenshot again to verify. It talks to Claude over stdio only
and never opens a network port.

Tested on Zorin OS 18.1 (GNOME Shell 46, Wayland). X11 is not supported.

## Two targets

Every observation and input tool takes a `target` parameter.

| `target` | What it drives | Effect on you |
|---|---|---|
| `sandbox` (default, recommended) | A nested `sway` compositor that appears as one window on your desktop. Apps launched with `sandbox_launch` run inside it and are driven through Wayland virtual-input protocols. | None. Your mouse and keyboard stay yours; you can keep working and move the sandbox window to another workspace. |
| `desktop` | Your real screen through the XDG Screenshot portal, and your real pointer/keyboard through a `/dev/uinput` virtual device. | It takes over the real cursor and keyboard. Do not touch the computer while a desktop-target action runs. |

The sandbox exists because Mutter (GNOME's compositor) has exactly one pointer
and one keyboard focus and offers no way to deliver input to a background
window. The desktop target is for the cases where Claude must operate windows
that already exist on your real desktop.

## Setup

Build the release binary:

```bash
cargo build --release
```

Install `sway` for the sandbox target and `wl-clipboard` for pasting text that
the US keyboard layout cannot type:

```bash
sudo apt install sway wl-clipboard
```

For the desktop target, allow your user to open `/dev/uinput` (udev rule plus
`input` group), then log out and back in:

```bash
sudo scripts/setup.sh
```

After logging in again, `scripts/check.sh` must report that `/dev/uinput` is
writable. `sudo scripts/uninstall.sh` reverses the udev and group changes.

Pre-authorize desktop screenshots so the portal never shows a permission
dialog. Run it once from the same environment that will launch Claude Code
(the grant is stored per launching application id):

```bash
scripts/grant-screenshot.sh
```

Register the server with Claude Code:

```bash
claude mcp add wayhand-mcp -- /absolute/path/to/target/release/wayhand-mcp
```

## Tools

| Tool | Purpose |
|---|---|
| `sandbox_start` / `sandbox_launch` / `sandbox_stop` | Start the nested sandbox desktop, launch a program inside it (argv array, no shell), stop it and its apps. |
| `screen_info` | Capture the target and report width, height and the coordinate system. |
| `screenshot` | Capture the target and return a PNG plus timestamp and size. |
| `move`, `click`, `double_click`, `right_click` | Pointer actions at `x,y` in screenshot pixels. |
| `drag` | Press at `from`, move through optional `waypoints`, release at `to`. |
| `scroll` | Wheel notches at `x,y`; positive `dy` scrolls down. |
| `key` | Key combo such as `ctrl+shift+t`, `alt+F4`, `Return`. |
| `type` | Type text; characters outside the US layout are pasted through the clipboard with `ctrl+v`. |
| `calibrate` | Desktop target only: moves the real cursor to known positions, finds it in screenshots, fits and stores the pixel→pointer transform, reports the worst residual against the 3 px limit. |

Coordinates are always pixel positions in the most recent screenshot of the
same target, origin top-left. Sandbox coordinates map 1:1 onto the virtual
pointer. Desktop coordinates are mapped linearly onto the uinput absolute axes.
Every action accepts `settle_ms` (default 150) to wait for the UI to settle.

Typical flow: `sandbox_start` → `sandbox_launch ["gnome-text-editor", "--standalone"]`
→ `screenshot` → `click` / `type` / `drag` → `screenshot` to verify.

## Safety

- Desktop-target actions move your real cursor and press your real keys. Keep
  your hands off the mouse and keyboard while they run.
- Tools never run a shell. `type` and `key` are pure key injection, and
  `sandbox_launch` executes the argv directly.
- `Ctrl+C` / `SIGINT` stops all further injection and releases every pressed
  button and key. Any injection error also releases pressed inputs.
- A circuit breaker refuses injection after 200 consecutive actions without a
  screenshot; a new screenshot resets it. Actions are serialized through one
  queue with a 20 ms minimum spacing, so calls never interleave.
- Only one server instance runs at a time (advisory lock at
  `$XDG_RUNTIME_DIR/wayhand-mcp.lock`). The server refuses to run as root.
- Desktop screenshots come from the portal as files under `$HOME`; the server
  reads a regular, user-owned file only (no symlinks, 64 MiB cap) and deletes
  it afterwards.

## Measured on the development machine

| Item | Value |
|---|---|
| Sandbox start (sway nested) | ~210 ms |
| Sandbox screenshot (1280×720, screencopy → PNG) | ~165 ms |
| Sandbox click including default 150 ms settle | ~152 ms |
| Desktop screenshot via portal, after permission grant | ~510 ms (2880×1800) |
| Desktop screenshot, first call with permission dialog | ~8.4 s |

## Test-only switches

```bash
WAYHAND_FAKE_INJECTOR=1 WAYHAND_SKIP_WAYLAND_CHECK=1 target/release/wayhand-mcp
```

`WAYHAND_FAKE_INJECTOR=1` records desktop input events in memory instead of
opening `/dev/uinput`. `WAYHAND_SKIP_WAYLAND_CHECK=1` bypasses the session
guard. Neither affects the sandbox target or the screenshot portal.

## Known limitations

- The sandbox only controls programs launched inside it through
  `sandbox_launch`. Windows already open on your real desktop need
  `target=desktop`.
- The sandbox window size is decided by GNOME (1280×720 or the available work
  area); the requested output mode is ignored by nested sway.
- GTK applications that are already running on the real desktop hand new
  launches to the existing instance over D-Bus, which would open the window on
  the real desktop. Pass a flag that forces a new process, for example
  `gnome-text-editor --standalone`.
- No input method is simulated. Text outside the US layout (Chinese, emoji,
  accented letters) is placed on the target's clipboard and pasted with
  `ctrl+v`, so it only works in apps that accept paste, and it replaces the
  clipboard content.
- Touchpad gestures (multi-finger swipe, pinch) cannot be produced.
- Desktop-target coordinates use a linear mapping; multi-monitor layouts and
  the sub-3-pixel calibration check are not verified yet.
- The uinput device is verified to build, but real desktop injection was not
  yet exercised on the development machine (pending a re-login into the
  `input` group).
- Sandbox screenshots include the cursor. Whether the GNOME portal screenshot includes it is unverified; if it does not, `calibrate` reports that it cannot see the cursor and the linear mapping stays in effect.

## Demo evidence

`docs/demo/` holds screenshots taken through the MCP tools during the sandbox
end-to-end run: the editor after launch, after `type`, after `ctrl+a`, and the
context menu after `right_click`. The clipboard content was read back with
`wl-paste` on the nested display and matched the typed and pasted text.
