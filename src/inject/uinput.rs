use std::collections::HashSet;

use anyhow::{Context, Result, anyhow};
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode,
    UinputAbsSetup, uinput::VirtualDevice,
};

use super::{Button, Injector};

const ABSOLUTE_MAX: i32 = 65_535;
const KEY_MAX: u16 = 0x02ff;

#[derive(Debug)]
pub struct UInputInjector {
    device: Option<VirtualDevice>,
    unavailable_reason: Option<String>,
    pressed_buttons: HashSet<Button>,
    pressed_keys: HashSet<u16>,
}

impl UInputInjector {
    pub fn new() -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 0..=KEY_MAX {
            keys.insert(KeyCode::new(code));
        }

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

        let device = VirtualDevice::builder()
            .context("open /dev/uinput")?
            .name("wayhand-mcp")
            .with_keys(&keys)
            .context("enable keyboard and button codes on uinput device")?
            .with_relative_axes(&relative_axes)
            .context("enable relative scroll axes on uinput device")?
            .with_absolute_axis(&x_axis)
            .context("enable ABS_X on uinput device")?
            .with_absolute_axis(&y_axis)
            .context("enable ABS_Y on uinput device")?
            .build()
            .context("create uinput virtual device")?;

        Ok(Self {
            device: Some(device),
            unavailable_reason: None,
            pressed_buttons: HashSet::new(),
            pressed_keys: HashSet::new(),
        })
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            device: None,
            unavailable_reason: Some(reason.into()),
            pressed_buttons: HashSet::new(),
            pressed_keys: HashSet::new(),
        }
    }

    fn device_mut(&mut self) -> Result<&mut VirtualDevice> {
        if let Some(device) = self.device.as_mut() {
            return Ok(device);
        }

        let reason = self
            .unavailable_reason
            .as_deref()
            .unwrap_or("the virtual device was not initialized");
        Err(anyhow!("uinput backend unavailable: {reason}"))
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
        self.device_mut()?.emit(&events)?;
        Ok(())
    }

    fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        let code = match button {
            Button::Left => KeyCode::BTN_LEFT,
            Button::Right => KeyCode::BTN_RIGHT,
            Button::Middle => KeyCode::BTN_MIDDLE,
        };
        let event = InputEvent::new(EventType::KEY.0, code.0, if pressed { 1 } else { 0 });
        let result = self.device_mut()?.emit(&[event]);
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
        let result = self.device_mut()?.emit(&[event]);
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
        self.device_mut()?.emit(&events)?;
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
        if let Err(error) = self.release_all() {
            tracing::error!(error = %error, "failed to release pressed inputs while dropping uinput injector");
        }
    }
}
