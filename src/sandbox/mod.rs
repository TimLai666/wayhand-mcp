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

pub struct Sandbox {
    options: SandboxOptions,
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

        let mut command = Command::new("sway");
        command
            .arg("-c")
            .arg(&config_path)
            .env("WAYHAND_SANDBOX_DIR", &dir);
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

        Ok(Self {
            options,
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

    pub fn options(&self) -> SandboxOptions {
        self.options
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
