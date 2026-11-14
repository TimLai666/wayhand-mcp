//! Sandbox target: a nested sway compositor shown as one GNOME window.
//!
//! Apps launched through `sandbox_launch` run inside it, so the user's own
//! pointer and keyboard are never touched.

pub mod wl;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};

use crate::{
    inject::{Button, Injector},
    keys::KeyMap,
};
use wl::WlClient;

/// How the sandbox compositor is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxOptions {
    /// Show the sandbox as a window on the real desktop (nested backend).
    /// Headless is the default: nothing is displayed and frames keep coming
    /// even when no window is visible.
    pub visible: bool,
    /// Output size; only honoured by the headless backend.
    pub width: u32,
    pub height: u32,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            visible: false,
            width: 1920,
            height: 1080,
        }
    }
}

fn sway_config(options: SandboxOptions) -> String {
    format!(
        "default_border none\n\
focus_follows_mouse no\n\
output * mode {}x{}\n\
output * bg #ff00ff solid_color\n\
exec sh -c 'printf %s \"$WAYLAND_DISPLAY\" > \"$WAYHAND_SANDBOX_DIR/display\"'\n",
        options.width, options.height
    )
}

/// A private AT-SPI bus for the sandbox: its own `dbus-daemon` with the
/// accessibility bus config plus `at-spi2-registryd`. Apps launched in the
/// sandbox get `AT_SPI_BUS_ADDRESS` pointing here, so the user's screen
/// reader never sees them and the tree is readable even when the session's
/// own accessibility bus is broken.
struct A11yBus {
    address: String,
    daemon: Child,
    registry: Child,
}

impl A11yBus {
    fn start(dir: &std::path::Path) -> Result<Self> {
        let socket = dir.join("a11y");
        let _ = std::fs::remove_file(&socket);
        let mut daemon = Command::new("dbus-daemon")
            .arg("--config-file=/usr/share/defaults/at-spi2/accessibility.conf")
            .arg("--nofork")
            .arg("--print-address")
            .arg(format!("--address=unix:path={}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start dbus-daemon for the sandbox accessibility bus (package dbus)")?;
        let stdout = daemon.stdout.take().context("dbus-daemon stdout")?;
        let stderr = daemon.stderr.take().context("dbus-daemon stderr")?;
        // The pipe read blocks, so it runs on a helper thread and the deadline
        // is enforced here; a daemon that neither prints nor exits is killed.
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            let mut line = String::new();
            let result = std::io::BufReader::new(stdout).read_line(&mut line);
            let _ = sender.send(result.map(|_| line));
        });
        let address = match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(line)) if !line.trim().is_empty() => line.trim().to_owned(),
            Ok(Ok(_)) => {
                return Err(anyhow!(
                    "dbus-daemon exited before printing its address: {}",
                    stop_child(&mut daemon, stderr)
                ));
            }
            Ok(Err(error)) => {
                let detail = stop_child(&mut daemon, stderr);
                return Err(anyhow!("read dbus-daemon address: {error}; {detail}"));
            }
            Err(_) => {
                let detail = stop_child(&mut daemon, stderr);
                return Err(anyhow!(
                    "dbus-daemon did not print its address within 5 seconds; {detail}"
                ));
            }
        };
        let registry = match Command::new("/usr/libexec/at-spi2-registryd")
            .env("AT_SPI_BUS_ADDRESS", &address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = daemon.kill();
                let _ = daemon.wait();
                return Err(error).context("start at-spi2-registryd (package at-spi2-core)");
            }
        };
        let mut bus = Self {
            address,
            daemon,
            registry,
        };
        // Apps register through org.a11y.atspi.Registry; one launched before
        // the registry owns that name never shows up, so wait for it. Dropping
        // `bus` on the error paths kills and reaps both daemons.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !bus.registry_ready() {
            if let Some(status) = bus.registry.try_wait().ok().flatten() {
                return Err(anyhow!("at-spi2-registryd exited with {status}"));
            }
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "at-spi2-registryd did not own org.a11y.atspi.Registry within 5 seconds"
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(bus)
    }

    fn registry_ready(&self) -> bool {
        Command::new("dbus-send")
            .arg(format!("--bus={}", self.address))
            .arg("--print-reply")
            .arg("--dest=org.freedesktop.DBus")
            .arg("/org/freedesktop/DBus")
            .arg("--reply-timeout=2000")
            .arg("org.freedesktop.DBus.NameHasOwner")
            .arg("string:org.a11y.atspi.Registry")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("boolean true")
            })
    }
}

impl Drop for A11yBus {
    fn drop(&mut self) {
        let _ = self.registry.kill();
        let _ = self.registry.wait();
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// Kill and reap a child, returning the first line of its stderr for the error.
fn stop_child(child: &mut Child, stderr: std::process::ChildStderr) -> String {
    use std::io::BufRead as _;
    let _ = child.kill();
    let _ = child.wait();
    let mut line = String::new();
    let _ = std::io::BufReader::new(stderr).read_line(&mut line);
    let line = line.trim();
    if line.is_empty() {
        "no stderr output".to_owned()
    } else {
        format!("stderr: {line}")
    }
}

pub struct Sandbox {
    options: SandboxOptions,
    compositor: Child,
    apps: Vec<Child>,
    display: String,
    dir: PathBuf,
    client: WlClient,
    /// None when the bus could not be started; `a11y_error` says why.
    a11y: Option<A11yBus>,
    a11y_error: Option<String>,
    pressed_buttons: HashSet<Button>,
    pressed_keys: HashSet<u16>,
    modifiers: u32,
}

impl Sandbox {
    /// `tag` names the runtime directory so two sandboxes (the working one
    /// and a calibration ruler) never share a display file.
    pub fn start(keymap: &KeyMap, options: SandboxOptions, tag: &str) -> Result<Self> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set; cannot start the sandbox"))?;
        if options.visible
            && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|value| value.is_empty())
        {
            return Err(anyhow!(
                "WAYLAND_DISPLAY is not set; the sandbox needs a parent Wayland session"
            ));
        }
        let dir = runtime_dir.join("wayhand-mcp").join(tag);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create sandbox directory {}", dir.display()))?;
        let display_file = dir.join("display");
        let _ = std::fs::remove_file(&display_file);
        let config_path = dir.join("sway.conf");
        std::fs::write(&config_path, sway_config(options))
            .with_context(|| format!("write sway config {}", config_path.display()))?;

        let sway_socket = dir.join("sway.sock");
        let _ = std::fs::remove_file(&sway_socket);
        let mut command = Command::new("sway");
        command
            .arg("-c")
            .arg(&config_path)
            .env("WAYHAND_SANDBOX_DIR", &dir)
            .env("SWAYSOCK", &sway_socket);
        if !options.visible {
            command
                .env("WLR_BACKENDS", "headless")
                .env("WLR_LIBINPUT_NO_DEVICES", "1")
                .env_remove("WAYLAND_DISPLAY");
        }
        let mut compositor = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start sway (is the `sway` package installed?)")?;

        let deadline = Instant::now() + Duration::from_secs(10);
        let display = loop {
            if let Some(status) = compositor.try_wait().context("poll sway")? {
                return Err(anyhow!("sway exited immediately with {status}"));
            }
            match std::fs::read_to_string(&display_file) {
                Ok(name) if !name.trim().is_empty() => break name.trim().to_owned(),
                _ if Instant::now() > deadline => {
                    let _ = compositor.kill();
                    return Err(anyhow!(
                        "sway did not publish its display name within 10 seconds"
                    ));
                }
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        };

        let socket_path = runtime_dir.join(&display);
        let client = match WlClient::connect(&socket_path, keymap.text()) {
            Ok(client) => client,
            Err(error) => {
                let _ = compositor.kill();
                let _ = compositor.wait();
                return Err(error);
            }
        };

        let (a11y, a11y_error) = match A11yBus::start(&dir) {
            Ok(bus) => (Some(bus), None),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "sandbox accessibility bus unavailable");
                (None, Some(format!("{error:#}")))
            }
        };

        Ok(Self {
            options,
            compositor,
            apps: Vec::new(),
            display,
            dir,
            client,
            a11y,
            a11y_error,
            pressed_buttons: HashSet::new(),
            pressed_keys: HashSet::new(),
            modifiers: 0,
        })
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    pub fn options(&self) -> SandboxOptions {
        self.options
    }

    pub fn client(&self) -> &WlClient {
        &self.client
    }

    /// D-Bus address of the sandbox's private accessibility bus, or the
    /// reason it is not available.
    pub fn a11y_address(&self) -> Result<&str, String> {
        match (&self.a11y, &self.a11y_error) {
            (Some(bus), _) => Ok(&bus.address),
            (None, Some(error)) => {
                Err(format!("sandbox accessibility bus is not running: {error}"))
            }
            (None, None) => Err("sandbox accessibility bus is not running".to_owned()),
        }
    }

    /// Each sandbox window's title and client area in output pixels, keyed
    /// by pid, read from sway IPC (bounded to 5 s by coreutils `timeout`).
    pub fn windows(&self) -> Result<HashMap<u32, Vec<crate::a11y::SwayWindow>>> {
        let output = Command::new("timeout")
            .arg("5")
            .arg("swaymsg")
            .arg("-s")
            .arg(self.dir.join("sway.sock"))
            .arg("-t")
            .arg("get_tree")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("run swaymsg -t get_tree")?;
        if !output.status.success() {
            return Err(anyhow!("swaymsg -t get_tree exited with {}", output.status));
        }
        crate::a11y::sway_windows(&String::from_utf8_lossy(&output.stdout))
            .map_err(|error| anyhow!(error))
    }

    pub fn launch(&mut self, argv: &[String], cwd: Option<&str>) -> Result<u32> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| anyhow!("argv must contain at least the program"))?;
        let mut command = Command::new(program);
        if let Some(bus) = &self.a11y {
            command.env("AT_SPI_BUS_ADDRESS", &bus.address);
        }
        command
            .args(args)
            .env("WAYLAND_DISPLAY", &self.display)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("GDK_BACKEND", "wayland")
            .env("QT_QPA_PLATFORM", "wayland")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let child = command
            .spawn()
            .with_context(|| format!("launch {program:?} inside the sandbox"))?;
        let pid = child.id();
        self.apps
            .retain_mut(|app| matches!(app.try_wait(), Ok(None)));
        self.apps.push(child);
        Ok(pid)
    }

    pub fn set_modifier_state(&mut self, modifiers: u32) {
        self.modifiers = modifiers;
    }

    fn stop_processes(&mut self) {
        for mut app in self.apps.drain(..) {
            let _ = app.kill();
            let _ = app.wait();
        }
        self.a11y = None;
        let _ = std::fs::remove_file(self.dir.join("a11y"));
        let _ = std::fs::remove_file(self.dir.join("sway.sock"));
        let pid = self.compositor.id();
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.compositor.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.compositor.kill();
        let _ = self.compositor.wait();
        let _ = std::fs::remove_file(self.dir.join("display"));
        let _ = std::fs::remove_file(self.dir.join("sway.conf"));
        let _ = std::fs::remove_dir(&self.dir);
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.release_all();
        self.stop_processes();
    }
}

fn button_code(button: Button) -> u32 {
    match button {
        Button::Left => wl::BTN_LEFT,
        Button::Right => wl::BTN_RIGHT,
        Button::Middle => wl::BTN_MIDDLE,
    }
}

impl Injector for Sandbox {
    fn move_abs(&mut self, x: u32, y: u32) -> Result<()> {
        self.client.motion(x, y)
    }

    fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        let result = self.client.button(button_code(button), pressed);
        if pressed {
            self.pressed_buttons.insert(button);
        } else if result.is_ok() {
            self.pressed_buttons.remove(&button);
        }
        result
    }

    fn key(&mut self, code: u16, pressed: bool) -> Result<()> {
        let result = self.client.key(code, pressed, self.modifiers);
        if pressed {
            self.pressed_keys.insert(code);
        } else if result.is_ok() {
            self.pressed_keys.remove(&code);
        }
        result
    }

    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<()> {
        self.client.scroll(horizontal, vertical)
    }

    fn release_all(&mut self) -> Result<()> {
        let mut first_error = None;
        for button in [Button::Left, Button::Right, Button::Middle] {
            if self.pressed_buttons.contains(&button)
                && let Err(error) = self.button(button, false)
            {
                first_error.get_or_insert(error);
            }
        }
        self.modifiers = 0;
        let mut keys: Vec<_> = self.pressed_keys.iter().copied().collect();
        keys.sort_unstable();
        for code in keys {
            if let Err(error) = self.key(code, false) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}
