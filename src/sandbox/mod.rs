//! Sandbox target: a nested sway compositor shown as one GNOME window.
//!
//! Apps launched through `sandbox_launch` run inside it, so the user's own
//! pointer and keyboard are never touched.

pub mod wl;

use std::{
    collections::HashSet,
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

const SWAY_CONFIG: &str = "default_border none\n\
focus_follows_mouse no\n\
exec sh -c 'printf %s \"$WAYLAND_DISPLAY\" > \"$WAYHAND_SANDBOX_DIR/display\"'\n";

pub struct Sandbox {
    compositor: Child,
    apps: Vec<Child>,
    display: String,
    dir: PathBuf,
    client: WlClient,
    pressed_buttons: HashSet<Button>,
    pressed_keys: HashSet<u16>,
    modifiers: u32,
}

impl Sandbox {
    pub fn start(keymap: &KeyMap) -> Result<Self> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set; cannot start the sandbox"))?;
        if std::env::var_os("WAYLAND_DISPLAY").is_none_or(|value| value.is_empty()) {
            return Err(anyhow!(
                "WAYLAND_DISPLAY is not set; the sandbox needs a parent Wayland session"
            ));
        }
        let dir = runtime_dir.join("wayhand-mcp");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create sandbox directory {}", dir.display()))?;
        let display_file = dir.join("display");
        let _ = std::fs::remove_file(&display_file);
        let config_path = dir.join("sway.conf");
        std::fs::write(&config_path, SWAY_CONFIG)
            .with_context(|| format!("write sway config {}", config_path.display()))?;

        let mut compositor = Command::new("sway")
            .arg("-c")
            .arg(&config_path)
            .env("WAYHAND_SANDBOX_DIR", &dir)
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

        Ok(Self {
            compositor,
            apps: Vec::new(),
            display,
            dir,
            client,
            pressed_buttons: HashSet::new(),
            pressed_keys: HashSet::new(),
            modifiers: 0,
        })
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    pub fn client(&self) -> &WlClient {
        &self.client
    }

    pub fn launch(&mut self, argv: &[String], cwd: Option<&str>) -> Result<u32> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| anyhow!("argv must contain at least the program"))?;
        let mut command = Command::new(program);
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
