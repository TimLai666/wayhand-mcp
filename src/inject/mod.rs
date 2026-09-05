#[allow(dead_code)]
pub mod fake;
pub mod uinput;

use anyhow::Result;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum Button {
    Left,
    Right,
    Middle,
}

#[allow(dead_code)]
pub trait Injector: Send {
    fn move_abs(&mut self, x: u16, y: u16) -> Result<()>;
    fn button(&mut self, button: Button, pressed: bool) -> Result<()>;
    fn key(&mut self, code: u16, pressed: bool) -> Result<()>;
    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<()>;
    fn release_all(&mut self) -> Result<()>;
}
