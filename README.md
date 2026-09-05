# wayhand-mcp

`desktop-driver` is a local MCP server for GNOME Wayland. It communicates with
Claude Code over stdio only and currently exposes `screen_info`, `screenshot`,
`move`, and `click`.

## Setup

Build the release binary:

```bash
cargo build --release
```

Allow the current user to open `/dev/uinput` and apply the udev rule:

```bash
sudo scripts/setup.sh
```

Log out and log back in so the `input` group membership takes effect, then
check the session:

```bash
scripts/check.sh
```

Pre-authorize the screenshot portal from the same environment that will launch
Claude Code:

```bash
scripts/grant-screenshot.sh
```

The script grants the empty app id and the app id derived from the current
process cgroup. If cgroup detection is unavailable, pass the app id explicitly,
for example `scripts/grant-screenshot.sh com.anthropic.Claude`. If this is not
run from the same environment, the first screenshot request shows the dialog
once; the answer is remembered separately for each app id.

Register the server with Claude Code:

```bash
claude mcp add desktop-driver -- /abs/path/target/release/desktop-driver
```

The server creates one uinput virtual device when it starts. Its absolute
pointer axes use the range `0..=65535`. Screenshot pixels are mapped linearly
to that range until calibration is implemented.

## Safety

This server controls the user's real mouse and keyboard. Do not touch the
computer while this server is running.

Press `Ctrl+C` or send `SIGINT` to stop all future input injection. The server
also refuses injection after 200 consecutive input operations without a
successful screenshot, and requires a 20 ms minimum interval between input
operations. A new successful screenshot resets the consecutive-operation
limit.

On an injection error, when the stop flag is set, and during injector shutdown,
the server releases every pressed button and key. One operation queue
serializes screenshots, screen information, and input so observations and
actions do not interleave.

Only one server instance may run at a time. It holds an advisory lock at
`$XDG_RUNTIME_DIR/wayhand-mcp.lock`, or `/tmp/wayhand-mcp-<uid>.lock` when
`XDG_RUNTIME_DIR` is unset; a second instance exits and names the lock path.
The server also refuses to start as root.

To reverse the udev and group changes, run `sudo scripts/uninstall.sh`, then
log out and back in again.

## Sandbox verification switches

These switches are test-only and must not be used for normal desktop
automation:

```bash
DESKTOP_DRIVER_FAKE_INJECTOR=1 \
DESKTOP_DRIVER_SKIP_WAYLAND_CHECK=1 \
target/release/desktop-driver
```

`DESKTOP_DRIVER_FAKE_INJECTOR=1` skips `/dev/uinput` and records input events
in memory. `DESKTOP_DRIVER_SKIP_WAYLAND_CHECK=1` bypasses the Wayland session
guard. They do not bypass the screenshot portal or make screenshot tools work
without a desktop session.

## Known limitations

- Only `screen_info`, `screenshot`, `move`, and `click` are implemented in
  this delivery step. The remaining tools and calibration are not implemented yet.
- The current coordinate mapping is linear. HiDPI calibration and the
  sub-three-pixel accuracy acceptance test are deferred to a later step.
- Run `scripts/grant-screenshot.sh` before automation to avoid the screenshot
  permission dialog. Without a matching app-id grant, the first request may
  still show it; the portal remembers the answer per app id.
- The development shell had no `/dev/uinput`, so no real pointer or button
  event was emitted. The real backend is present, while unit tests use only the
  fake backend.
- When GNOME `scaling-factor` is `0` (automatic), `screen_info` reports
  `scaleFactor: null` with an explanatory note; non-zero values are reported as
  numbers and do not change the screenshot-pixel coordinate system.
- The screenshot portal's temporary file is validated, read, decoded as PNG,
  and deleted before the tool returns. The end-to-end text-editor and clipboard
  demo is deferred until live uinput access is available.
