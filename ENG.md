# Engineering Plan: wayhand-mcp
_2026-09-04 - eng-architect - wayhand-mcp:main_

## Architecture

```text
[Claude Code] --stdio JSON-RPC--> [wayhand-mcp (rmcp ServerHandler)]
                                     |
                 +-------------------+--------------------+
                 |                   |                    |
        [screenshot.rs]        [server.rs tools]      [safety.rs Budget]
        ashpd -> XDG portal    validate args          SIGINT stop flag
        org.freedesktop.       coords::Transform      circuit breaker (200)
        portal.Screenshot      -> Injector            min interval 20ms
        reads + deletes PNG          |                settle_ms per call
                                     v
                          [inject::Injector trait]
                          /                     \
                 [inject/uinput.rs]        [inject/fake.rs]
                 one long-lived            Vec<FakeEvent>
                 /dev/uinput device        for unit tests
                 ABS_X/ABS_Y 0..=65535
                 all KEY codes, REL_WHEEL

Trust boundary: everything is local. stdio only, no sockets, no network.
```

## Data flow

1. `screenshot` / `screen_info`: portal request -> file URI -> read bytes -> delete file -> PNG header gives width/height -> remember size -> reset circuit breaker -> return image + timestamp.
2. `move` / `click`: require a remembered screenshot size -> validate x,y inside it -> `Transform::linear` maps pixel to ABS 0..=65535 -> `Budget::before_injection` (stop flag, breaker, throttle) -> `Injector` emits events -> wait `settle_ms`.

## Two targets (decided 2026-09-04)

```text
target=sandbox (default, recommended in tool descriptions)
  [wayhand-mcp] --spawn--> [sway, nested wayland backend, shown as one GNOME window]
       |  wayland-client on the nested socket
       |    zwlr_virtual_pointer_v1  -> pointer motion_absolute/button/axis
       |    zwp_virtual_keyboard_v1  -> keys with an xkb keymap
       |    zwlr_screencopy_v1       -> screenshot of the nested output
       +-- sandbox_launch spawns apps with WAYLAND_DISPLAY=<nested socket>
  Coordinates: screenshot pixels == virtual pointer extent, no calibration.

target=desktop
  [wayhand-mcp] --uinput--> Mutter seat (steals the real cursor)
                   --portal--> org.freedesktop.portal.Screenshot
  Coordinates: screenshot pixels -> ABS 0..=65535 via Transform, calibrate later.
```

Mutter has one seat and no protocol to deliver input to an unfocused window, so the sandbox is the only way to keep the user's cursor free. Both targets share tools, validation, safety budget and the `Injector` trait; each target has its own injector and screenshot source.

## Decisions

- Injection writes `/dev/uinput` directly (evdev crate). ydotool was dropped: apt ships 0.1.8 without absolute pointer moves. The server itself is the long-lived virtual device, so there is no daemon or socket.
- Screenshot pixels are the only coordinate system exposed to the client. Physical panel is 2880x1800 on the dev machine; the portal returns physical pixels, so the linear ABS mapping is scale-independent. `calibrate` will replace `Transform::linear` with a measured affine matrix.
- GNOME `scaling-factor` from gsettings returns 0 when automatic. It is informational only and must not feed the coordinate math.

## Test seams

| Seam | What gets faked | Why this level |
|---|---|---|
| `Injector` trait | the uinput device (`FakeInjector` records events) | tool logic, breaker and ordering are testable without hardware |
| `coords::Transform` | nothing, pure function | corner, rounding and range tests |
| `safety::Budget` | time via `with_config` | breaker and stop flag tests |
| stdio JSON-RPC | none, run the binary with `WAYHAND_FAKE_INJECTOR=1` | proves the MCP contract (initialize, tools/list, tools/call) |

Real uinput and the portal are exercised only in the manual demo on the developer's desktop.

## Test matrix

| Test type | What to cover | Priority |
|---|---|---|
| Unit | coords mapping, settle bounds, breaker trip/reset, stop flag, fake event order | P0 |
| Integration | binary over stdio with fake injector: tools/list, tools/call error paths | P0 |
| Manual E2E | text editor demo with screenshots, clipboard check via wl-paste | P1 |

## Measured

| Item | Value |
|---|---|
| portal screenshot, first call | 8384 ms (2026-09-04) |
| portal screenshot, second call | 509 ms |
| screenshot size | 2880x1800, ~740 KB PNG |
| portal output path | `~/Pictures/Screenshot.png`, deleted by the server after reading |
| sandbox start | ~210 ms (2026-09-05) |
| sandbox screenshot 1280x720 | ~165 ms |
| sandbox click incl. 150 ms settle | ~152 ms |
| sandbox demo | typed + pasted text verified through wl-paste on the nested display |

## Hidden assumptions

- The portal keeps answering without a dialog after the first grant - risk: if GNOME prompts every call, the loop needs the ScreenCast portal instead. Unverified as of 2026-09-04.
- libinput accepts a uinput device advertising ABS_X/ABS_Y plus every key code as a pointer - risk: it may classify it as a tablet or touchscreen and drop button events. Verified only after re-login with real uinput.
- Only one client drives the server at a time - risk: concurrent tool calls share one injector mutex and one screen size, ordering is not guaranteed across clients.
