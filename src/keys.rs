//! Keyboard mapping shared by both targets.
//!
//! Every key is expressed as a Linux evdev code. The XKB keycode used by the
//! virtual keyboard protocol is `evdev + 8`. The US layout is compiled once at
//! startup with xkbcommon; its text form is what the sandbox keyboard installs.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use xkbcommon::xkb;

pub const XKB_OFFSET: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

impl Modifier {
    fn xkb_name(self) -> &'static str {
        match self {
            Self::Ctrl => xkb::MOD_NAME_CTRL,
            Self::Shift => xkb::MOD_NAME_SHIFT,
            Self::Alt => xkb::MOD_NAME_ALT,
            Self::Super => xkb::MOD_NAME_LOGO,
        }
    }

    fn keysym_name(self) -> &'static str {
        match self {
            Self::Ctrl => "Control_L",
            Self::Shift => "Shift_L",
            Self::Alt => "Alt_L",
            Self::Super => "Super_L",
        }
    }

    fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some(Self::Ctrl),
            "shift" => Some(Self::Shift),
            "alt" | "option" => Some(Self::Alt),
            "super" | "meta" | "win" | "cmd" | "logo" => Some(Self::Super),
            _ => None,
        }
    }
}

/// One key press: the evdev code and whether Shift must be held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStroke {
    pub evdev: u16,
    pub shift: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combo {
    pub modifiers: Vec<Modifier>,
    pub key: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextRun {
    Typed(Vec<KeyStroke>),
    Pasted(String),
}

pub struct KeyMap {
    text: String,
    /// keysym (raw) -> stroke, lowest shift level wins.
    by_keysym: HashMap<u32, KeyStroke>,
    modifier_masks: HashMap<Modifier, u32>,
}

impl KeyMap {
    pub fn us() -> Result<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow!("xkbcommon could not compile the US keymap"))?;
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);

        let mut by_keysym = HashMap::new();
        for level in 0..=1u32 {
            for keycode in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
                let Some(evdev) = keycode
                    .checked_sub(XKB_OFFSET)
                    .and_then(|code| u16::try_from(code).ok())
                else {
                    continue;
                };
                for sym in keymap.key_get_syms_by_level(xkb::Keycode::new(keycode), 0, level) {
                    by_keysym.entry(sym.raw()).or_insert(KeyStroke {
                        evdev,
                        shift: level == 1,
                    });
                }
            }
        }

        let mut modifier_masks = HashMap::new();
        for modifier in [
            Modifier::Ctrl,
            Modifier::Shift,
            Modifier::Alt,
            Modifier::Super,
        ] {
            let index = keymap.mod_get_index(modifier.xkb_name());
            let mask = if index == xkb::MOD_INVALID {
                0
            } else {
                1 << index
            };
            modifier_masks.insert(modifier, mask);
        }

        Ok(Self {
            text,
            by_keysym,
            modifier_masks,
        })
    }

    /// XKB keymap text, installed on the sandbox virtual keyboard.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Keystroke that produces `c` on the US layout, if any.
    pub fn char_stroke(&self, c: char) -> Option<KeyStroke> {
        let sym = match c {
            '\n' | '\r' => xkb::keysym_from_name("Return", xkb::KEYSYM_NO_FLAGS),
            '\t' => xkb::keysym_from_name("Tab", xkb::KEYSYM_NO_FLAGS),
            _ => xkb::utf32_to_keysym(c as u32),
        };
        self.by_keysym.get(&sym.raw()).copied()
    }

    /// Evdev code for a keysym name such as `Return`, `F4`, `a`, `Escape`.
    pub fn named_key(&self, name: &str) -> Option<u16> {
        for candidate in [name.to_owned(), name.to_ascii_lowercase()] {
            let sym = xkb::keysym_from_name(&candidate, xkb::KEYSYM_CASE_INSENSITIVE);
            if sym.raw() == 0 {
                continue;
            }
            if let Some(stroke) = self.by_keysym.get(&sym.raw()) {
                return Some(stroke.evdev);
            }
        }
        None
    }

    pub fn modifier_evdev(&self, modifier: Modifier) -> Result<u16> {
        self.named_key(modifier.keysym_name())
            .ok_or_else(|| anyhow!("US keymap has no key for {}", modifier.keysym_name()))
    }

    /// XKB modifier mask for the virtual keyboard `modifiers` request.
    pub fn modifier_mask(&self, modifiers: &[Modifier]) -> u32 {
        modifiers
            .iter()
            .map(|modifier| self.modifier_masks.get(modifier).copied().unwrap_or(0))
            .fold(0, |mask, bit| mask | bit)
    }

    /// Parse `ctrl+shift+t`, `alt+F4`, `Return`, `super`.
    pub fn parse_combo(&self, combo: &str) -> Result<Combo> {
        let mut modifiers = Vec::new();
        let mut key = None;
        let tokens: Vec<&str> = combo
            .split('+')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .collect();
        if tokens.is_empty() {
            return Err(anyhow!("key combo is empty"));
        }
        for (index, token) in tokens.iter().enumerate() {
            let is_last = index + 1 == tokens.len();
            if let Some(modifier) = Modifier::parse(token) {
                if is_last && tokens.len() == 1 {
                    key = Some(self.modifier_evdev(modifier)?);
                } else if modifiers.contains(&modifier) {
                    return Err(anyhow!("modifier {token} repeated in {combo:?}"));
                } else {
                    modifiers.push(modifier);
                }
                continue;
            }
            if !is_last {
                return Err(anyhow!(
                    "{token:?} in {combo:?} is not a modifier; only the last token may be a key"
                ));
            }
            key = Some(
                self.named_key(token)
                    .ok_or_else(|| anyhow!("unknown key name {token:?} in {combo:?}"))?,
            );
        }
        let key = key.ok_or_else(|| anyhow!("key combo {combo:?} has no key"))?;
        Ok(Combo { modifiers, key })
    }

    /// Split text into runs that can be typed on the US layout and runs that
    /// must be pasted through the clipboard.
    pub fn split_text(&self, text: &str) -> Vec<TextRun> {
        let mut runs: Vec<TextRun> = Vec::new();
        for c in text.chars() {
            match self.char_stroke(c) {
                Some(stroke) => match runs.last_mut() {
                    Some(TextRun::Typed(strokes)) => strokes.push(stroke),
                    _ => runs.push(TextRun::Typed(vec![stroke])),
                },
                None => match runs.last_mut() {
                    Some(TextRun::Pasted(existing)) => existing.push(c),
                    _ => runs.push(TextRun::Pasted(c.to_string())),
                },
            }
        }
        runs
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyMap, KeyStroke, Modifier, TextRun};

    const KEY_A: u16 = 30;
    const KEY_LEFTSHIFT: u16 = 42;
    const KEY_LEFTCTRL: u16 = 29;
    const KEY_T: u16 = 20;
    const KEY_ENTER: u16 = 28;
    const KEY_F4: u16 = 62;

    fn keymap() -> KeyMap {
        KeyMap::us().unwrap()
    }

    #[test]
    fn maps_ascii_letters_with_shift_level() {
        let map = keymap();
        assert_eq!(
            map.char_stroke('a'),
            Some(KeyStroke {
                evdev: KEY_A,
                shift: false
            })
        );
        assert_eq!(
            map.char_stroke('A'),
            Some(KeyStroke {
                evdev: KEY_A,
                shift: true
            })
        );
        assert_eq!(
            map.char_stroke('\n'),
            Some(KeyStroke {
                evdev: KEY_ENTER,
                shift: false
            })
        );
        assert_eq!(map.char_stroke('!').map(|s| s.shift), Some(true));
    }

    #[test]
    fn chinese_is_not_typeable_on_us_layout() {
        assert_eq!(keymap().char_stroke('中'), None);
    }

    #[test]
    fn parses_combos() {
        let map = keymap();
        let combo = map.parse_combo("ctrl+shift+t").unwrap();
        assert_eq!(combo.modifiers, vec![Modifier::Ctrl, Modifier::Shift]);
        assert_eq!(combo.key, KEY_T);
        assert_eq!(map.parse_combo("Return").unwrap().key, KEY_ENTER);
        assert_eq!(map.parse_combo("alt+F4").unwrap().key, KEY_F4);
        assert_eq!(map.parse_combo("shift").unwrap().key, KEY_LEFTSHIFT);
        assert_eq!(map.modifier_evdev(Modifier::Ctrl).unwrap(), KEY_LEFTCTRL);
    }

    #[test]
    fn rejects_bad_combos() {
        let map = keymap();
        assert!(map.parse_combo("").is_err());
        assert!(map.parse_combo("ctrl+nosuchkey").is_err());
        assert!(map.parse_combo("t+ctrl").is_err());
        assert!(map.parse_combo("ctrl+ctrl+t").is_err());
    }

    #[test]
    fn splits_text_into_typed_and_pasted_runs() {
        let runs = keymap().split_text("ab中文c");
        assert_eq!(runs.len(), 3);
        assert!(matches!(&runs[0], TextRun::Typed(strokes) if strokes.len() == 2));
        assert_eq!(runs[1], TextRun::Pasted("中文".to_owned()));
        assert!(matches!(&runs[2], TextRun::Typed(strokes) if strokes.len() == 1));
    }

    #[test]
    fn modifier_mask_is_nonzero_for_known_modifiers() {
        let map = keymap();
        let mask = map.modifier_mask(&[Modifier::Ctrl, Modifier::Shift]);
        assert_eq!(mask.count_ones(), 2);
    }
}
