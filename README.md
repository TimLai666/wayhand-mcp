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
| `sandbox` (default, recommended) | A private `sway` compositor, headless by default (nothing appears on your screen; `sandbox_start {"visible": true}` shows it as a window instead). Apps launched with `sandbox_launch` run inside it and are driven through Wayland virtual-input protocols. | None. Your mouse and keyboard stay yours and you can keep working. |
| `desktop` | Your real screen through the XDG Screenshot portal, and your real pointer/keyboard through a `/dev/uinput` virtual device. | It takes over the real cursor and keyboard. Do not touch the computer while a desktop-target action runs. |

The sandbox exists because Mutter (GNOME's compositor) has exactly one pointer
and one keyboard focus and offers no way to deliver input to a background
window. The desktop target is for the cases where Claude must operate windows
that already exist on your real desktop.

## Quick install

One command downloads the latest release, installs `sway` and `wl-clipboard`
(asks for sudo), sets up `/dev/uinput` access, pre-authorizes screenshots, and
registers the server with every supported client it finds (`claude`, `codex`):

```bash
curl -fsSL https://raw.githubusercontent.com/TimLai666/wayhand-mcp/main/scripts/install.sh | bash
```

Options: `bash -s -- --claude` or `--codex` to register only one client,
`--skip-sudo` to leave the system steps for later, `--version vX.Y.Z` to pin
a release. Releases are x86_64 Linux tarballs with a `.sha256` next to them.

## Codex

The same server works with OpenAI Codex CLI. The installer runs
`codex mcp add wayhand-mcp -- ~/.local/share/wayhand-mcp/wayhand-mcp`; to do
it by hand, use that command or add to `~/.codex/config.toml`:

```toml
[mcp_servers.wayhand-mcp]
command = "/home/you/.local/share/wayhand-mcp/wayhand-mcp"
```

Codex launches MCP servers on the host, outside its own sandbox, so the
uinput and portal access work the same way as under Claude Code.

## Manual setup

Build the release binary:

```bash
cargo build --release
```

Install `sway` for the sandbox target and `wl-clipboard` for pasting text that
the US keyboard layout cannot type:

```bash
sudo apt install sway wl-clipboard
```

For the desktop target, allow your user to open `/dev/uinput`. The udev rule
tags the device `uaccess`, so logind grants the logged-in user an ACL right
away; membership in the `input` group is added as a fallback for non-seat
sessions and only takes effect after the user session restarts.

```bash
sudo scripts/setup.sh
```

`scripts/check.sh` must then report that `/dev/uinput` is writable.
`sudo scripts/uninstall.sh` reverses the udev and group changes.

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
| `sandbox_start` / `sandbox_launch` / `sandbox_stop` | Start the sandbox desktop (headless 1920×1080 by default, or `visible: true` for a window on the real desktop; `width`/`height` for headless), launch a program inside it (argv array, no shell), stop it and its apps. |
| `screen_info` | Capture the target and report width, height and the coordinate system. |
| `screenshot` | Capture the target and return a PNG plus timestamp and size. |
| `move`, `click`, `double_click`, `right_click` | Pointer actions at `x,y` in screenshot pixels. |
| `drag` | Press at `from`, move through optional `waypoints`, release at `to`. |
| `scroll` | Wheel notches at `x,y`; positive `dy` scrolls down. |
| `key` | Key combo such as `ctrl+shift+t`, `alt+F4`, `Return`. |
| `type` | Type text; characters outside the US layout are pasted through the clipboard with `ctrl+v`. |
| `calibrate` | Desktop target only: opens a temporary magenta ruler window, moves the real cursor to known positions inside it, reads the cursor back from the ruler, and stores the verified pixel→pointer mapping with the worst deviation against the 3 px limit. |

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
  button and key. Any injection error, and a cancelled MCP request, also
  release pressed inputs.
- A circuit breaker refuses injection after 200 consecutive actions without a
  screenshot; a new screenshot resets it. Actions are serialized through one
  queue with a 20 ms minimum spacing, so calls never interleave.
- Only one server instance may drive the real desktop at a time (advisory
  lock at `$XDG_RUNTIME_DIR/wayhand-mcp.lock`, taken when the uinput devices
  are created); other instances can still use the sandbox target. The server
  refuses to run as root.
- Desktop screenshots come from the portal as files under `$HOME`; the server
  reads a regular, user-owned file only (no symlinks, 64 MiB cap) and deletes
  it afterwards.

## Measured on the development machine

| Item | Value |
|---|---|
| Sandbox start (sway nested) | ~210 ms |
| Sandbox screenshot (1280×720 visible window) | ~165 ms |
| Sandbox screenshot (1920×1080 headless, screencopy → PNG) | ~20-35 ms empty screen, ~165 ms with an app window |
| Headless sandbox start | ~110-210 ms |
| Sandbox click including default 150 ms settle | ~152 ms |
| Desktop screenshot via portal, after permission grant | ~510-670 ms (2880×1800) |
| Desktop screenshot, first call with permission dialog | ~8.4 s |
| Desktop move / click / key via uinput | ~1 ms plus the 20 ms spacing |
| `calibrate` on the desktop target | ~2.6 s, worst deviation 0.00 px at 2× scale |

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
- A visible sandbox window only receives frames from GNOME while it is on
  screen and not covered; screenshots of a covered, minimized or
  other-workspace sandbox window time out after 5 s. The headless sandbox
  (default) has no such limit. The visible window's size is decided by GNOME.
- GTK applications that are already running on the real desktop hand new
  launches to the existing instance over D-Bus, which would open the window on
  the real desktop. Pass a flag that forces a new process, for example
  `gnome-text-editor --standalone`.
- No input method is simulated. Text outside the US layout (Chinese, emoji,
  accented letters) is placed on the target's clipboard and pasted with
  `ctrl+v`, so it only works in apps that accept paste, and it replaces the
  clipboard content.
- Touchpad gestures (multi-finger swipe, pinch) cannot be produced.
- Desktop-target coordinates use a linear mapping over one screen;
  multi-monitor layouts are not handled.
- Desktop screenshots from the GNOME portal do not include the cursor, which
  is why `calibrate` measures through the sandbox window instead.
- The clipboard fallback of `type` writes to the compositor of the chosen
  `target`. With `target=desktop` the text lands on the real desktop clipboard,
  so pasting into an app that runs inside the sandbox does not work that way.

## Demo evidence

`docs/demo/` holds screenshots taken through the MCP tools during the sandbox
end-to-end run: the editor after launch, after `type`, after `ctrl+a`, and the
context menu after `right_click`. The clipboard content was read back with
`wl-paste` on the nested display and matched the typed and pasted text.

The desktop target was exercised the same way with the real pointer and
keyboard devices, using the sandbox window as the application under test:
`click` into it, `type`, `ctrl+a`, `drag`, `scroll`, `right_click` all
arrived (`docs/demo/desktop-*.png`), and `calibrate` measured the linear
mapping at 0.00 px deviation.
