//! The interface the terminal front end drives: a device, an emulator, or a
//! browser page.

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Home,
    Switcher,
    Back,
    Lock,
    Center,
    Notify,
    VolDown,
    VolUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialKey {
    Enter,
    Esc,
    Backspace,
    Tab,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

/// A live target shown in one terminal pane.
pub trait Target {
    /// Native pixel size, used to keep the aspect ratio.
    fn size(&self) -> (usize, usize);

    /// New RGBA frame at the requested size, or None when nothing changed.
    fn frame_rgba(&mut self, width: usize, height: usize) -> Option<Vec<u8>>;

    fn action(&mut self, action: Action);

    /// Normalized 0..1 coordinates.
    fn touch(&mut self, x: f64, y: f64, down: bool);

    fn swipe(&mut self, x1: f64, y1: f64, x2: f64, y2: f64);

    fn char(&mut self, c: char, shift: bool, ctrl: bool);

    fn special(&mut self, key: SpecialKey);

    /// Called when the pane's pixel area changes, so adapters that render a
    /// free-form page can update their viewport. Devices keep their size.
    fn resize(&mut self, _width: usize, _height: usize) {}

    fn shutdown(&mut self) {}
}
