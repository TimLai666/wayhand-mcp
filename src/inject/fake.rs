use std::collections::HashSet;

use super::{Button, Injector};
use anyhow::{Result, anyhow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeEvent {
    MoveAbs { x: u32, y: u32 },
    Button { button: Button, pressed: bool },
    Key { code: u16, pressed: bool },
    Scroll { horizontal: i32, vertical: i32 },
    Syn,
}

#[derive(Debug, Default)]
pub struct FakeInjector {
    pub events: Vec<FakeEvent>,
    pub pressed_buttons: HashSet<Button>,
    pub pressed_keys: HashSet<u16>,
    fail_next: Option<String>,
    fail_after: Option<usize>,
}

impl FakeInjector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fail_next(&mut self, reason: impl Into<String>) {
        self.fail_next = Some(reason.into());
    }

    /// Let `successes` more calls succeed, then fail the one after them.
    pub fn fail_after(&mut self, successes: usize) {
        self.fail_after = Some(successes);
    }

    fn record(&mut self, event: FakeEvent) -> Result<()> {
        self.events.push(event);
        self.events.push(FakeEvent::Syn);
        if let Some(remaining) = self.fail_after.as_mut() {
            if *remaining == 0 {
                self.fail_after = None;
                return Err(anyhow!("fake injector failure"));
            }
            *remaining -= 1;
        }
        self.fail_next
            .take()
            .map_or(Ok(()), |reason| Err(anyhow!(reason)))
    }
}

impl Injector for FakeInjector {
    fn move_abs(&mut self, x: u32, y: u32) -> Result<()> {
        self.record(FakeEvent::MoveAbs { x, y })
    }

    fn button(&mut self, button: Button, pressed: bool) -> Result<()> {
        if pressed {
            self.pressed_buttons.insert(button);
        }
        let result = self.record(FakeEvent::Button { button, pressed });
        if !pressed && result.is_ok() {
            self.pressed_buttons.remove(&button);
        }
        result
    }

    fn key(&mut self, code: u16, pressed: bool) -> Result<()> {
        if pressed {
            self.pressed_keys.insert(code);
        }
        let result = self.record(FakeEvent::Key { code, pressed });
        if !pressed && result.is_ok() {
            self.pressed_keys.remove(&code);
        }
        result
    }

    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<()> {
        self.record(FakeEvent::Scroll {
            horizontal,
            vertical,
        })
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

#[cfg(test)]
mod tests {
    use super::{FakeEvent, FakeInjector};
    use crate::inject::{Button, Injector};

    #[test]
    fn records_move_event_and_sync() {
        let mut fake = FakeInjector::new();

        fake.move_abs(123, 456).unwrap();

        assert_eq!(
            fake.events,
            vec![FakeEvent::MoveAbs { x: 123, y: 456 }, FakeEvent::Syn]
        );
    }

    #[test]
    fn records_click_press_release_with_sync_between_each_event() {
        let mut fake = FakeInjector::new();

        fake.button(Button::Left, true).unwrap();
        fake.button(Button::Left, false).unwrap();

        assert_eq!(
            fake.events,
            vec![
                FakeEvent::Button {
                    button: Button::Left,
                    pressed: true,
                },
                FakeEvent::Syn,
                FakeEvent::Button {
                    button: Button::Left,
                    pressed: false,
                },
                FakeEvent::Syn,
            ]
        );
        assert!(fake.pressed_buttons.is_empty());
    }

    #[test]
    fn failed_release_is_recorded_and_can_be_retried_by_release_all() {
        let mut fake = FakeInjector::new();

        fake.button(Button::Left, true).unwrap();
        fake.fail_next("release failed");
        assert!(fake.button(Button::Left, false).is_err());
        assert_eq!(
            &fake.events[2..],
            &[
                FakeEvent::Button {
                    button: Button::Left,
                    pressed: false,
                },
                FakeEvent::Syn,
            ]
        );

        fake.release_all().unwrap();

        assert!(fake.pressed_buttons.is_empty());
    }

    #[test]
    fn release_all_releases_every_pressed_button_and_key() {
        let mut fake = FakeInjector::new();

        fake.button(Button::Right, true).unwrap();
        fake.key(42, true).unwrap();
        fake.release_all().unwrap();

        assert!(fake.pressed_buttons.is_empty());
        assert!(fake.pressed_keys.is_empty());
        assert_eq!(
            &fake.events[4..],
            &[
                FakeEvent::Button {
                    button: Button::Right,
                    pressed: false,
                },
                FakeEvent::Syn,
                FakeEvent::Key {
                    code: 42,
                    pressed: false,
                },
                FakeEvent::Syn,
            ]
        );
    }
}
