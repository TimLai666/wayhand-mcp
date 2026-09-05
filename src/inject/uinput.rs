use std::collections::HashSet;

use anyhow::{Context, Result, anyhow};
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode,
    UinputAbsSetup, uinput::VirtualDevice,
};

use super::{Button, Injector};

const ABSOLUTE_MAX: i32 = 65_535;

/// Highest keyboard key code advertised on the keyboard device (KEY_* range).
/// BTN_* codes (0x100..) are deliberately excluded: advertising BTN_TOOL_PEN or
/// BTN_TOUCH next to ABS_X/ABS_Y makes udev classify the device as a tablet or
/// touchscreen, and Mutter then ignores plain absolute motion.
const KEYBOARD_KEY_MAX: u16 = 0xff;

#[derive(Debug)]
pub struct UInputInjector {
    /// Absolute pointer shaped like a QEMU usb-tablet: ABS_X/ABS_Y, three
    /// mouse buttons and wheel axes. libinput treats it as an absolute mouse.
    pointer: Option<VirtualDevice>,
    keyboard: Option<VirtualDevice>,
    unavailable_reason: Option<String>,
    pressed_buttons: HashSet<Button>,
    pressed_keys: HashSet<u16>,
}

impl UInputInjector {
    pub fn new() -> Result<Self> {
        let mut buttons = AttributeSet::<KeyCode>::new();
        buttons.insert(KeyCode::BTN_LEFT);
        buttons.insert(KeyCode::BTN_RIGHT);
        buttons.insert(KeyCode::BTN_MIDDLE);

        let mut relative_axes = AttributeSet::<RelativeAxisCode>::new();
        relative_axes.insert(RelativeAxisCode::REL_WHEEL);
        relative_axes.insert(RelativeAxisCode::REL_HWHEEL);

        let x_axis = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            AbsInfo::new(0, 0, ABSOLUTE_MAX, 0, 0, 0),
        );
        let y_axis = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            AbsInfo::new(0, 0, ABSOLUTE_MAX, 0, 0, 0),
        );

        let pointer = VirtualDevice::builder()
            .context("open /dev/uinput")?
            .name("wayhand-mcp pointer")
            .with_keys(&buttons)
            .context("enable mouse buttons on uinput pointer")?
            .with_relative_axes(&relative_axes)
            .context("enable scroll axes on uinput pointer")?
            .with_absolute_axis(&x_axis)
            .context("enable ABS_X on uinput pointer")?
            .with_absolute_axis(&y_axis)
            .context("enable ABS_Y on uinput pointer")?
            .build()
            .context("create uinput pointer device")?;

        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=KEYBOARD_KEY_MAX {
            keys.insert(KeyCode::new(code));
        }
        let keyboard = VirtualDevice::builder()
            .context("open /dev/uinput for keyboard")?
            .name("wayhand-mcp keyboard")
            .with_keys(&keys)
            .context("enable key codes on uinput keyboard")?
            .build()
            .context("create uinput keyboard device")?;

        Ok(Self {
            pointer: Some(pointer),
            keyboard: Some(keyboard),
            unavailable_reason: None,
            pressed_buttons: HashSet::new(),
            pressed_keys: HashSet::new(),
        })
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            pointer: None,
            keyboard: None,
            unavailable_reason: Some(reason.into()),
            pressed_buttons: HashSet::new(),
            pressed_keys: HashSet::new(),
        }
    }

    /// Retry opening the devices when an earlier attempt failed, so a server
    /// started before `/dev/uinput` was writable recovers without a restart.
    fn ensure_devices(&mut self) -> Result<()> {
        if self.pointer.is_some() && self.keyboard.is_some() {
            return Ok(());
        }
        match Self::new() {
            Ok(mut fresh) => {
                tracing::info!("uinput became available; virtual devices created");
                self.pointer = fresh.pointer.take();
                self.keyboard = fresh.keyboard.take();
                self.unavailable_reason = None;
                Ok(())
            }
            Err(error) => {
                let reason = format!("{error:#}");
                self.unavailable_reason = Some(reason.clone());
                Err(anyhow!(
                    "uinput backend unavailable: {reason} (run scripts/setup.sh and scripts/check.sh)"
                ))
            }
        }
    }

    fn pointer_mut(&mut self) -> Result<&mut VirtualDevice> {
        self.ensure_devices()?;
        self.pointer
            .as_mut()
            .ok_or_else(|| anyhow!("uinput pointer device missing"))
    }

    fn keyboard_mut(&mut self) -> Result<&mut VirtualDevice> {
        self.ensure_devices()?;
        self.keyboard
            .as_mut()
            .ok_or_else(|| anyhow!("uinput keyboard device missing"))
    }
}

impl Injector for UInputInjector {
    fn move_abs(&mut self, x: u32, y: u32) -> Result<()> {
        let x = i32::try_from(x).map_err(|_| anyhow!("x coordinate exceeds uinput range"))?;
        let y = i32::try_from(y).map_err(|_| anyhow!("y coordinate exceeds uinput range"))?;
        if x > ABSOLUTE_MAX || y > ABSOLUTE_MAX {
            return Err(anyhow!(
                "coordinate exceeds uinput range 0..={ABSOLUTE_MAX}"
            ));
        }
        let events = [
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, x),
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, y),
        ];
        self.pointer_mut()?.emit(&events)?;
        Ok(())
    }

    fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        let code = match button {
            Button::Left => KeyCode::BTN_LEFT,
            Button::Right => KeyCode::BTN_RIGHT,
            Button::Middle => KeyCode::BTN_MIDDLE,
        };
        let event = InputEvent::new(EventType::KEY.0, code.0, if pressed { 1 } else { 0 });
        let result = self.pointer_mut()?.emit(&[event]);
        if pressed {
            self.pressed_buttons.insert(button);
        } else if result.is_ok() {
            self.pressed_buttons.remove(&button);
        }
        result?;
        Ok(())
    }

    fn key(&mut self, code: u16, pressed: bool) -> Result<()> {
        let event = InputEvent::new(EventType::KEY.0, code, if pressed { 1 } else { 0 });
        let result = self.keyboard_mut()?.emit(&[event]);
        if pressed {
            self.pressed_keys.insert(code);
        } else if result.is_ok() {
            self.pressed_keys.remove(&code);
        }
        result?;
        Ok(())
    }

    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<()> {
        let events = [
            InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_HWHEEL.0,
                horizontal,
            ),
            InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_WHEEL.0,
                vertical,
            ),
        ];
        self.pointer_mut()?.emit(&events)?;
        Ok(())
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

impl Drop for UInputInjector {
    fn drop(&mut self) {
        if self.pointer.is_none() && self.keyboard.is_none() {
            return;
        }
        if let Err(error) = self.release_all() {
            tracing::error!(error = %error, "failed to release pressed inputs while dropping uinput injector");
        }
    }
}
