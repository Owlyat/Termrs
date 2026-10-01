//! Key representation, `config.toml` spec parsing, matching, PTY encoding.
//!
//! Backend-agnostic on purpose: [`from_winit`] converts window events into a
//! [`KeyPress`], and everything else (bindings, PTY bytes) works on that type.

use std::ops::{BitOr, BitOrAssign};

use winit::event::ElementState;
use winit::event::KeyEvent as WinitKeyEvent;
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

/// Modifier flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods(u8);

impl Mods {
    pub const CONTROL: Mods = Mods(1);
    pub const ALT: Mods = Mods(2);
    pub const SHIFT: Mods = Mods(4);
    pub const SUPER: Mods = Mods(8);

    /// No modifiers.
    pub const fn empty() -> Self {
        Mods(0)
    }

    /// Whether every flag in `other` is set here.
    pub const fn contains(self, other: Mods) -> bool {
        self.0 & other.0 == other.0
    }

    /// Set every flag in `other`.
    pub fn insert(&mut self, other: Mods) {
        self.0 |= other.0;
    }

    /// Clear every flag in `other`.
    pub fn remove(&mut self, other: Mods) {
        self.0 &= !other.0;
    }

    /// Whether no flag is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for Mods {
    type Output = Mods;
    fn bitor(self, rhs: Mods) -> Mods {
        Mods(self.0 | rhs.0)
    }
}

impl BitOrAssign for Mods {
    fn bitor_assign(&mut self, rhs: Mods) {
        self.0 |= rhs.0;
    }
}

/// A logical key, independent of any windowing backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Space,
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Insert,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

/// Whether a key event is an initial press, an auto-repeat, or a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyKind {
    #[default]
    Press,
    Repeat,
    Release,
}

/// One key press: the key, its modifiers, and any text it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPress {
    pub key: Key,
    pub mods: Mods,
    /// Text produced by the press (respects layout/shift); `None` for
    /// control combos and pure navigation keys.
    pub text: Option<String>,
    /// Press / auto-repeat / release.
    pub kind: KeyKind,
}

/// Parsed key binding: key plus required modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub key: Key,
    pub mods: Mods,
}

/// Parse `ctrl+shift+a`, `alt+enter`, `f5`, `esc`, `up`, `space`, `tab`, `a`.
pub fn parse(spec: &str) -> Option<Binding> {
    let spec = spec.trim().to_lowercase();
    if spec.is_empty() {
        return None;
    }
    let mut mods = Mods::empty();
    let mut key: Option<Key> = None;
    for part in spec.split('+') {
        let p = part.trim();
        match p {
            "ctrl" | "control" => mods |= Mods::CONTROL,
            "alt" => mods |= Mods::ALT,
            "shift" => mods |= Mods::SHIFT,
            "super" | "win" | "meta" => mods |= Mods::SUPER,
            "" => return None,
            other => {
                if key.is_some() {
                    return None;
                }
                key = Some(parse_key(other)?);
            }
        }
    }
    Some(Binding { key: key?, mods })
}

/// Single key token -> [`Key`].
fn parse_key(key: &str) -> Option<Key> {
    match key {
        "enter" | "return" => Some(Key::Enter),
        "esc" | "escape" => Some(Key::Esc),
        "tab" => Some(Key::Tab),
        "backtab" => Some(Key::BackTab),
        "backspace" => Some(Key::Backspace),
        "space" | "spacebar" => Some(Key::Space),
        "plus" | "add" => Some(Key::Char('+')),
        "minus" | "subtract" => Some(Key::Char('-')),
        "equals" | "equal" => Some(Key::Char('=')),
        "up" | "up_arrow" => Some(Key::Up),
        "down" | "down_arrow" => Some(Key::Down),
        "left" | "left_arrow" => Some(Key::Left),
        "right" | "right_arrow" => Some(Key::Right),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" | "pgup" => Some(Key::PageUp),
        "pagedown" | "pgdn" => Some(Key::PageDown),
        "insert" | "ins" => Some(Key::Insert),
        "delete" | "del" => Some(Key::Delete),
        s if s.starts_with('f') => s[1..].parse::<u8>().ok().map(Key::F),
        s if s.chars().count() == 1 => s.chars().next().map(Key::Char),
        _ => None,
    }
}

/// Match a press against any spec in a list (one action, many bindings).
pub fn matches_any(press: &KeyPress, specs: &[String]) -> bool {
    specs.iter().any(|s| matches(press, s))
}

/// Match a press against a spec string. SHIFT is ignored for character keys,
// since the character itself already carries the shift.
pub fn matches(press: &KeyPress, spec: &str) -> bool {
    let Some(b) = parse(spec) else { return false };
    // With ctrl held the OS may report either case (`b` vs `B`); the
    // binding means the same physical key either way.
    if press.mods.contains(Mods::CONTROL) || b.mods.contains(Mods::CONTROL) {
        if let (Key::Char(a), Key::Char(c)) = (press.key, b.key) {
            if !a.eq_ignore_ascii_case(&c) {
                return false;
            }
        } else if press.key != b.key {
            return false;
        }
    } else if press.key != b.key {
        return false;
    }
    let mut pm = press.mods;
    let mut bm = b.mods;
    // SHIFT is ignored for character keys because the character already carries
    // it (`ctrl+B` == `ctrl+b`). A spec that explicitly asks for SHIFT is an
    // exception, so `ctrl+r` and `ctrl+shift+r` stay distinct.
    if matches!(press.key, Key::Char(_) | Key::Space) && !bm.contains(Mods::SHIFT) {
        pm.remove(Mods::SHIFT);
        bm.remove(Mods::SHIFT);
    }
    // On Windows, Ctrl+Alt is AltGr and the platform may report the press
    // with ALT only (CONTROL dropped). For bindings that ask for both, treat
    // CONTROL as optional so `ctrl+alt+...` still matches.
    if bm.contains(Mods::CONTROL) && bm.contains(Mods::ALT) {
        pm.remove(Mods::CONTROL);
        bm.remove(Mods::CONTROL);
    }
    pm == bm
}

/// Encode a key press as bytes for the PTY child.
///
/// Navigation keys carry their modifiers using the xterm CSI form
/// (`ESC [ 1 ; <n> <final>`), so `ctrl+left`/`ctrl+right` move by word and
/// `shift+arrow` selects, as shells/readline expect. `ctrl+backspace` sends
/// `0x17` (kill word backward) and `ctrl+delete` the forward equivalent.
pub fn to_bytes(press: &KeyPress) -> Vec<u8> {
    let ctrl = press.mods.contains(Mods::CONTROL);
    let alt = press.mods.contains(Mods::ALT);
    let mut out = Vec::new();
    match press.key {
        Key::Enter => out.push(b'\r'),
        Key::Backspace => {
            if ctrl {
                out.push(0x17); // kill word backward
            } else {
                if alt {
                    out.push(0x1b);
                }
                out.push(0x7f);
            }
        }
        Key::Tab => out.push(b'\t'),
        Key::BackTab => out.extend_from_slice(b"\x1b[Z"),
        Key::Esc => out.push(0x1b),
        // CSI navigation keys, modifier-aware.
        Key::Up => csi(b'A', press.mods, &mut out),
        Key::Down => csi(b'B', press.mods, &mut out),
        Key::Right => csi(b'C', press.mods, &mut out),
        Key::Left => csi(b'D', press.mods, &mut out),
        Key::Home => csi(b'H', press.mods, &mut out),
        Key::End => csi(b'F', press.mods, &mut out),
        Key::PageUp => csi_tilde(5, press.mods, &mut out),
        Key::PageDown => csi_tilde(6, press.mods, &mut out),
        Key::Insert => csi_tilde(2, press.mods, &mut out),
        Key::Delete => csi_tilde(3, press.mods, &mut out),
        Key::F(n) => out.extend_from_slice(format!("\x1b[{n}~").as_bytes()),
        Key::Char(c) => {
            if alt {
                out.push(0x1b);
            }
            encode_char(c, ctrl, alt, press.text.as_deref(), &mut out);
        }
        Key::Space => {
            if alt {
                out.push(0x1b);
            }
            encode_char(' ', ctrl, alt, press.text.as_deref(), &mut out);
        }
    }
    out
}

/// xterm modifier parameter: 1 + shift*1 + alt*2 + ctrl*4 + super*8.
fn modifier_param(m: Mods) -> u8 {
    let mut n = 1u8;
    if m.contains(Mods::SHIFT) {
        n += 1;
    }
    if m.contains(Mods::ALT) {
        n += 2;
    }
    if m.contains(Mods::CONTROL) {
        n += 4;
    }
    if m.contains(Mods::SUPER) {
        n += 8;
    }
    n
}

/// `ESC [ <final>` or `ESC [ 1 ; <mod> <final>` when modifiers are held.
fn csi(final_byte: u8, mods: Mods, out: &mut Vec<u8>) {
    let p = modifier_param(mods);
    if p == 1 {
        out.extend_from_slice(&[0x1b, b'[', final_byte]);
    } else {
        out.extend_from_slice(format!("\x1b[1;{p}").as_bytes());
        out.push(final_byte);
    }
}

/// `ESC [ <n> ~` or `ESC [ <n> ; <mod> ~` when modifiers are held.
fn csi_tilde(n: u8, mods: Mods, out: &mut Vec<u8>) {
    let p = modifier_param(mods);
    if p == 1 {
        out.extend_from_slice(format!("\x1b[{n}~").as_bytes());
    } else {
        out.extend_from_slice(format!("\x1b[{n};{p}~").as_bytes());
    }
}

/// Encode a printable key, honouring control bytes and produced text.
fn encode_char(c: char, ctrl: bool, alt: bool, text: Option<&str>, out: &mut Vec<u8>) {
    if ctrl && !alt {
        // Already a C0 control byte (e.g. reported verbatim): pass through.
        if (c as u32) < 0x20 {
            out.push(c as u8);
            return;
        }
        if let Some(b) = ctrl_byte(c) {
            out.push(b);
            return;
        }
    }
    match text {
        Some(t) if !t.is_empty() => out.extend_from_slice(t.as_bytes()),
        _ => {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
}

/// `ctrl+a` -> 0x01 ... `ctrl+z` -> 0x1a; `ctrl+space` -> 0x00.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        ' ' => Some(0x00),
        'a'..='z' => Some((c as u8) - b'a' + 1),
        'A'..='Z' => Some((c as u8) - b'A' + 1),
        '[' | '{' => Some(0x1b),
        '\\' | '|' => Some(0x1c),
        ']' | '}' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// Encode one key event in ConPTY's `win32-input-mode`
/// (`CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`).
///
/// ConPTY always asks the terminal to switch into this mode at startup
/// (`CSI ? 9001 h`); honouring it is what gives the hosted console app real
/// key-down/key-up timing (a plain VT encoding only ever delivers the press,
/// so apps can never observe a held key -- which is why sliders in `osutty`
/// misbehave). Virtual key codes come from the OS keyboard layout; scan codes
/// are derived from them.
#[cfg(windows)]
pub fn to_win32_bytes(press: &KeyPress) -> Vec<u8> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
    let vk = virtual_key(press);
    if vk == 0 {
        return Vec::new();
    }
    let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 };
    let uc = unicode_char(press);
    // Navigation/editing keys sit on the "enhanced" keyboard half; the bit
    // lets ConPTY tell them apart from their numeric-keypad twins.
    let enhanced = matches!(
        press.key,
        Key::Up | Key::Down | Key::Left | Key::Right | Key::Home | Key::End
            | Key::PageUp | Key::PageDown | Key::Insert | Key::Delete
    );
    let cs = control_key_state(press.mods) | if enhanced { 0x0100 } else { 0 };
    let kd = u8::from(press.kind != KeyKind::Release);
    format!("\x1b[{vk};{sc};{uc};{kd};{cs};1_").into_bytes()
}

/// Windows virtual key code for a press (0 when unknown).
#[cfg(windows)]
fn virtual_key(press: &KeyPress) -> u16 {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW;
    if let Key::Char(c) = press.key {
        let u = c as u32;
        if u > 0xFFFF {
            return 0;
        }
        let r = unsafe { VkKeyScanW(u as u16) };
        return if r == -1 { 0 } else { (r as u16) & 0xFF };
    }
    match press.key {
        Key::Space => 0x20,
        Key::Enter => 0x0D,
        Key::Esc => 0x1B,
        Key::Tab | Key::BackTab => 0x09,
        Key::Backspace => 0x08,
        Key::Delete => 0x2E,
        Key::Insert => 0x2D,
        Key::Up => 0x26,
        Key::Down => 0x28,
        Key::Left => 0x25,
        Key::Right => 0x27,
        Key::Home => 0x24,
        Key::End => 0x23,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::F(n) => 0x70 + (n as u16).saturating_sub(1),
        Key::Char(_) => 0,
    }
}

/// UTF-16 code unit carried by the event (the produced text, else the key's
/// natural character); 0 for pure navigation keys.
#[cfg(windows)]
fn unicode_char(press: &KeyPress) -> u16 {
    // A win32 KEY_EVENT_RECORD for Ctrl+<letter> carries the *control
    // character* (Ctrl+P -> 0x10), not the base letter. ConPTY translates the
    // record straight back to VT, so sending 'p' here makes a hosted TUI see a
    // plain 'p' instead of Ctrl+P.
    if press.mods.contains(Mods::CONTROL) {
        return match press.key {
            Key::Char(c) => ctrl_byte(c).map(u16::from).unwrap_or(0),
            _ => 0,
        };
    }
    if let Some(ch) = press.text.as_deref().and_then(|t| t.chars().next()) {
        let mut buf = [0u16; 2];
        return ch.encode_utf16(&mut buf)[0];
    }
    match press.key {
        Key::Space => 0x20,
        Key::Enter => 0x0D,
        Key::Esc => 0x1B,
        Key::Tab | Key::BackTab => 0x09,
        Key::Backspace => 0x08,
        Key::Char(c) if (c as u32) < 0x10000 => c as u16,
        _ => 0,
    }
}

/// `dwControlKeyState` bits (left-hand variants; the hosted app cannot tell
/// sides apart from a winit event). SHIFT / CTRL / ALT.
#[cfg(windows)]
fn control_key_state(mods: Mods) -> u32 {
    let mut cs = 0u32;
    if mods.contains(Mods::SHIFT) {
        cs |= 0x0010;
    }
    if mods.contains(Mods::CONTROL) {
        cs |= 0x0008;
    }
    if mods.contains(Mods::ALT) {
        cs |= 0x0002;
    }
    cs
}

/// Whether a named key + modifiers is an OS shortcut that must never reach
/// the shell. Alt+Tab is the window-switch combo: Windows delivers the Tab
/// press to the app before stealing focus, and the stray `\t` would land in
/// the shell's input line.
fn is_os_shortcut(key: &WinitKey, mods: Mods) -> bool {
    matches!(key, WinitKey::Named(NamedKey::Tab)) && mods.contains(Mods::ALT)
}

/// Choose the key to classify. Windows reports Alt/AltGr+letter combos as
/// `Unidentified` on layouts with no mapping for that Alt level (e.g.
/// Ctrl+Alt+Shift+S on an AltGr layout). The modifier-free layout character is
/// still available in `without_modifiers`, so use it as a fallback instead of
/// dropping the press; a resolvable logical key (including a real AltGr
/// character) always wins.
fn resolve_logical(logical: &WinitKey, without_modifiers: &WinitKey) -> WinitKey {
    match logical {
        WinitKey::Unidentified(_) => without_modifiers.clone(),
        other => other.clone(),
    }
}

/// Convert a winit key event into a [`KeyPress`], using `mods` tracked from
/// `WindowEvent::ModifiersChanged`.
pub fn from_winit(ev: &WinitKeyEvent, mods: Mods) -> Option<KeyPress> {
    if is_os_shortcut(&ev.logical_key, mods) {
        return None;
    }
    let mut mods = mods;
    let logical = resolve_logical(&ev.logical_key, &ev.key_without_modifiers());
    let key = match &logical {
        WinitKey::Named(named) => match named {
            NamedKey::Enter => Key::Enter,
            NamedKey::Escape => Key::Esc,
            NamedKey::Tab if mods.contains(Mods::ALT) => return None,
            NamedKey::Tab if mods.contains(Mods::SHIFT) => Key::BackTab,
            NamedKey::Tab => Key::Tab,
            NamedKey::Backspace => Key::Backspace,
            NamedKey::Delete => Key::Delete,
            NamedKey::Insert => Key::Insert,
            NamedKey::ArrowUp => Key::Up,
            NamedKey::ArrowDown => Key::Down,
            NamedKey::ArrowLeft => Key::Left,
            NamedKey::ArrowRight => Key::Right,
            NamedKey::Home => Key::Home,
            NamedKey::End => Key::End,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            NamedKey::Space => Key::Space,
            NamedKey::F1 => Key::F(1),
            NamedKey::F2 => Key::F(2),
            NamedKey::F3 => Key::F(3),
            NamedKey::F4 => Key::F(4),
            NamedKey::F5 => Key::F(5),
            NamedKey::F6 => Key::F(6),
            NamedKey::F7 => Key::F(7),
            NamedKey::F8 => Key::F(8),
            NamedKey::F9 => Key::F(9),
            NamedKey::F10 => Key::F(10),
            NamedKey::F11 => Key::F(11),
            NamedKey::F12 => Key::F(12),
            NamedKey::F13 => Key::F(13),
            NamedKey::F14 => Key::F(14),
            NamedKey::F15 => Key::F(15),
            NamedKey::F16 => Key::F(16),
            NamedKey::F17 => Key::F(17),
            NamedKey::F18 => Key::F(18),
            NamedKey::F19 => Key::F(19),
            NamedKey::F20 => Key::F(20),
            NamedKey::F21 => Key::F(21),
            NamedKey::F22 => Key::F(22),
            NamedKey::F23 => Key::F(23),
            NamedKey::F24 => Key::F(24),
            _ => return None,
        },
        WinitKey::Character(s) => {
            let c = s.chars().next()?;
            let (k, m) = classify_char(c, mods)?;
            mods = m;
            k
        },
        _ => return None,
    };
    let text = ev.text.as_ref().map(|t| t.to_string());
    let kind = match ev.state {
        ElementState::Pressed if ev.repeat => KeyKind::Repeat,
        ElementState::Pressed => KeyKind::Press,
        ElementState::Released => KeyKind::Release,
    };
    Some(KeyPress {
        key,
        mods,
        text,
        kind,
    })
}

/// Map a character produced by the platform to a key + modifiers.
///
/// Handles two platform quirks: `ctrl+letter` delivered as a C0 control
/// character (with or without the CONTROL flag), and space delivered as a
/// regular character. Returns `None` for nothing usable.
fn classify_char(c: char, mods: Mods) -> Option<(Key, Mods)> {
    if c == ' ' {
        return Some((Key::Space, mods));
    }
    if (c as u32) < 0x20 {
        let mut cmods = mods;
        cmods.insert(Mods::CONTROL);
        let key = match c {
            '\0' => Key::Space,
            '\t' => Key::Tab,
            '\r' => Key::Enter,
            '\u{1b}' => Key::Esc,
            '\u{8}' | '\u{7f}' => Key::Backspace,
            _ => Key::Char(control_to_letter(c)),
        };
        return Some((key, cmods));
    }
    if mods.contains(Mods::CONTROL) {
        return Some((Key::Char(control_to_letter(c)), mods));
    }
    Some((Key::Char(c), mods))
}

/// Map a C0 control character to its letter (`U+000F` -> `o`), so bindings
/// match no matter which representation the platform reports. Passes other
/// characters through unchanged.
fn control_to_letter(c: char) -> char {
    let n = c as u32;
    if (0x01..=0x1a).contains(&n) {
        char::from_u32(n + 0x60).unwrap_or(c)
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key, mods: Mods) -> KeyPress {
        KeyPress {
            key,
            mods,
            text: None,
            kind: KeyKind::Press,
        }
    }

    #[test]
    fn control_chars_normalize_to_letters() {
        assert_eq!(control_to_letter('\u{f}'), 'o');
        assert_eq!(control_to_letter('\u{1}'), 'a');
        assert_eq!(control_to_letter('x'), 'x');
        // Binding matches either representation.
        assert!(matches(
            &press(Key::Char('o'), Mods::CONTROL),
            "ctrl+o"
        ));
    }

    #[test]
    fn classify_char_handles_all_representations() {
        // ctrl+o as a bare C0 char, no CONTROL flag: gains CONTROL.
        let (k, m) = classify_char('\u{f}', Mods::empty()).unwrap();
        assert_eq!(k, Key::Char('o'));
        assert!(m.contains(Mods::CONTROL));
        assert!(matches(
            &KeyPress {
                key: k,
                mods: m,
                text: None,
                kind: KeyKind::Press,
            },
            "ctrl+o"
        ));
        // ctrl+o as the letter with CONTROL.
        let (k, m) = classify_char('o', Mods::CONTROL).unwrap();
        assert_eq!(k, Key::Char('o'));
        assert!(m.contains(Mods::CONTROL));
        // Plain letter and space pass through.
        assert_eq!(classify_char('v', Mods::empty()).unwrap().0, Key::Char('v'));
        assert_eq!(classify_char(' ', Mods::empty()).unwrap().0, Key::Space);
        // ctrl+space (NUL) becomes Space + CONTROL.
        let (k, m) = classify_char('\0', Mods::empty()).unwrap();
        assert_eq!(k, Key::Space);
        assert!(m.contains(Mods::CONTROL));
    }

    #[test]
    fn ctrl_alt_tolerates_altgr() {
        // AltGr: press reports ALT only, spec asks for CONTROL|ALT.
        let mut only_alt = Mods::empty();
        only_alt.insert(Mods::ALT);
        assert!(matches(
            &press(Key::Char('s'), only_alt),
            "ctrl+alt+s"
        ));
        // Normal ctrl+alt still matches too.
        let mut both = Mods::empty();
        both.insert(Mods::CONTROL);
        both.insert(Mods::ALT);
        assert!(matches(&press(Key::Char('s'), both), "ctrl+alt+s"));
        // But alt-only must not satisfy a plain ctrl binding.
        assert!(!matches(&press(Key::Char('s'), only_alt), "ctrl+s"));
    }

    /// Ctrl+Alt+Shift+<letter> on an AltGr layout arrives with an
    /// `Unidentified` logical key; the modifier-free fallback must resolve it
    /// so the binding is not dropped.
    #[test]
    fn altgr_unidentified_uses_modifier_free_key() {
        use winit::keyboard::NativeKey;

        let unidentified = WinitKey::Unidentified(NativeKey::Unidentified);
        let base = WinitKey::Character("s".into());
        assert_eq!(resolve_logical(&unidentified, &base), base);

        // A resolvable logical key is kept (real AltGr characters survive).
        let euro = WinitKey::Character("\u{20ac}".into());
        assert_eq!(resolve_logical(&euro, &base), euro);

        // The resolved Char('s') plus control/shift/alt matches the binding.
        let mut mods = Mods::empty();
        mods.insert(Mods::CONTROL);
        mods.insert(Mods::SHIFT);
        mods.insert(Mods::ALT);
        assert!(matches(&press(Key::Char('s'), mods), "ctrl+shift+alt+s"));
    }

    #[test]
    fn select_mode_binding_matches_ctrl_shift_space() {
        let mut mods = Mods::empty();
        mods.insert(Mods::CONTROL);
        mods.insert(Mods::SHIFT);
        let p = KeyPress {
            key: Key::Space,
            mods,
            text: None,
            kind: KeyKind::Press,
        };
        assert!(matches(&p, "ctrl+shift+space"));
    }

    #[test]
    fn zoom_specs_parse() {
        assert_eq!(parse("ctrl+plus").unwrap().key, Key::Char('+'));
        assert_eq!(parse("ctrl+minus").unwrap().key, Key::Char('-'));
        assert_eq!(parse("ctrl+0").unwrap().key, Key::Char('0'));
    }

    #[test]
    fn arrow_aliases() {
        assert_eq!(parse_key("right_arrow"), Some(Key::Right));
        assert_eq!(parse_key("left_arrow"), Some(Key::Left));
        assert_eq!(parse_key("up_arrow"), Some(Key::Up));
        assert_eq!(parse_key("down_arrow"), Some(Key::Down));
        assert!(matches(
            &press(Key::Right, Mods::CONTROL | Mods::SHIFT),
            "ctrl+shift+right_arrow"
        ));
    }

    #[test]
    fn parse_ctrl_q() {
        let b = parse("ctrl+q").unwrap();
        assert_eq!(b.key, Key::Char('q'));
        assert!(b.mods.contains(Mods::CONTROL));
    }

    #[test]
    fn ctrl_binding_ignores_case() {
        assert!(matches(
            &press(Key::Char('b'), Mods::CONTROL),
            "ctrl+B"
        ));
        assert!(matches(
            &press(Key::Char('B'), Mods::CONTROL | Mods::SHIFT),
            "ctrl+b"
        ));
    }

    #[test]
    fn matches_any_hits_each_binding() {
        let specs = vec!["ctrl+q".to_string(), "alt+q".to_string()];
        assert!(matches_any(&press(Key::Char('q'), Mods::CONTROL), &specs));
        assert!(matches_any(&press(Key::Char('q'), Mods::ALT), &specs));
        assert!(!matches_any(&press(Key::Char('q'), Mods::empty()), &specs));
        let empty: Vec<String> = Vec::new();
        assert!(!matches_any(&press(Key::Char('q'), Mods::CONTROL), &empty));
    }

    #[test]
    fn match_ignores_char_shift() {
        assert!(matches(
            &press(Key::Char('q'), Mods::CONTROL),
            "ctrl+q"
        ));
        assert!(!matches(
            &press(Key::Char('q'), Mods::CONTROL),
            "ctrl+w"
        ));
    }

    /// A spec that asks for SHIFT requires it, so ctrl+r and ctrl+shift+r are
    /// different bindings (while ctrl+B still matches ctrl+b).
    #[test]
    fn explicit_shift_spec_is_required() {
        assert!(matches(
            &press(Key::Char('r'), Mods::CONTROL | Mods::SHIFT),
            "ctrl+shift+r"
        ));
        assert!(!matches(
            &press(Key::Char('r'), Mods::CONTROL),
            "ctrl+shift+r"
        ));
    }

    #[test]
    fn shift_pageup_spec() {
        let b = parse("shift+pageup").unwrap();
        assert_eq!(b.key, Key::PageUp);
        assert!(b.mods.contains(Mods::SHIFT));
        assert!(matches(&press(Key::PageUp, Mods::SHIFT), "shift+pageup"));
    }

    /// Alt+Tab is an OS window-switch shortcut: Windows delivers the Tab
    /// press to the app before stealing focus, and the stray `\t` would land
    /// in the shell's input line. Swallow it so it never reaches the pane.
    #[test]
    fn alt_tab_is_swallowed() {
        use winit::keyboard::{Key as WinitKey, NamedKey};

        let tab = WinitKey::Named(NamedKey::Tab);
        let mut alt = Mods::empty();
        alt.insert(Mods::ALT);
        assert!(is_os_shortcut(&tab, alt));
        assert!(!is_os_shortcut(&tab, Mods::empty()));
        let mut shift = Mods::empty();
        shift.insert(Mods::SHIFT);
        assert!(!is_os_shortcut(&tab, shift));
        // Other keys are not affected.
        let esc = WinitKey::Named(NamedKey::Escape);
        assert!(!is_os_shortcut(&esc, alt));
    }

    #[test]
    fn ctrl_bytes() {
        assert_eq!(ctrl_byte('a'), Some(1));
        assert_eq!(ctrl_byte('q'), Some(17));
        assert_eq!(to_bytes(&press(Key::Enter, Mods::empty())), vec![b'\r']);
        assert_eq!(to_bytes(&press(Key::Up, Mods::empty())), b"\x1b[A".to_vec());
    }

    /// Navigation keys carry modifiers (word movement, selection).
    #[test]
    fn modifier_aware_navigation() {
        let ctrl = |k| press(k, Mods::CONTROL);
        assert_eq!(to_bytes(&ctrl(Key::Left)), b"\x1b[1;5D".to_vec());
        assert_eq!(to_bytes(&ctrl(Key::Right)), b"\x1b[1;5C".to_vec());
        assert_eq!(
            to_bytes(&press(Key::Up, Mods::SHIFT)),
            b"\x1b[1;2A".to_vec()
        );
        assert_eq!(
            to_bytes(&press(Key::Right, Mods::CONTROL | Mods::SHIFT)),
            b"\x1b[1;6C".to_vec()
        );
        assert_eq!(to_bytes(&ctrl(Key::Delete)), b"\x1b[3;5~".to_vec());
        assert_eq!(to_bytes(&press(Key::PageUp, Mods::empty())), b"\x1b[5~".to_vec());
    }

    /// Ctrl+Backspace deletes the previous word; plain/alt backspace differ.
    #[test]
    fn ctrl_backspace_kills_word() {
        assert_eq!(to_bytes(&press(Key::Backspace, Mods::CONTROL)), vec![0x17]);
        assert_eq!(to_bytes(&press(Key::Backspace, Mods::empty())), vec![0x7f]);
        assert_eq!(to_bytes(&press(Key::Backspace, Mods::ALT)), vec![0x1b, 0x7f]);
    }

    #[test]
    fn text_is_preferred_for_printables() {
        let p = KeyPress {
            key: Key::Char('A'),
            mods: Mods::SHIFT,
            text: Some("A".into()),
            kind: KeyKind::Press,
        };
        assert_eq!(to_bytes(&p), b"A".to_vec());
    }

    #[test]
    fn alt_prefixes_escape() {
        let p = KeyPress {
            key: Key::Char('x'),
            mods: Mods::ALT,
            text: Some("x".into()),
            kind: KeyKind::Press,
        };
        assert_eq!(to_bytes(&p), b"\x1bx".to_vec());
    }

    /// `win32-input-mode` records carry Vk/Sc/Uc/Kd/Cs/Rc; press has Kd=1 and
    /// release Kd=0 so a held key (slider) can be observed by the child.
    #[cfg(windows)]
    #[test]
    fn win32_bytes_encode_press_and_release() {
        let params = |kind| {
            let p = KeyPress {
                key: Key::Char('z'),
                mods: Mods::empty(),
                text: Some("z".into()),
                kind,
            };
            let text = String::from_utf8(to_win32_bytes(&p)).unwrap();
            let body = text.strip_prefix("\x1b[").unwrap().strip_suffix("_").unwrap();
            body.split(';').map(str::to_string).collect::<Vec<_>>()
        };
        let down = params(KeyKind::Press);
        let up = params(KeyKind::Release);
        assert_eq!(down.len(), 6);
        assert_eq!(up.len(), 6);
        assert_eq!(down[2], "122"); // UnicodeChar 'z'
        assert_eq!(up[2], "122");
        assert_eq!(down[3], "1"); // bKeyDown
        assert_eq!(up[3], "0"); // key up
        assert!(down[0].parse::<u32>().unwrap() > 0, "virtual key code present");
    }

    /// Ctrl+<letter> must carry the control character (Ctrl+P -> 0x10), not the
    /// base letter, or ConPTY hands a plain letter to the hosted app.
    #[cfg(windows)]
    #[test]
    fn win32_ctrl_letter_uses_control_char() {
        let p = KeyPress {
            key: Key::Char('p'),
            mods: Mods::CONTROL,
            text: Some("p".into()),
            kind: KeyKind::Press,
        };
        let text = String::from_utf8(to_win32_bytes(&p)).unwrap();
        let body = text.strip_prefix("\x1b[").unwrap().strip_suffix("_").unwrap();
        let params: Vec<&str> = body.split(';').collect();
        assert_eq!(params[2], "16", "Ctrl+P must send UnicodeChar 0x10");
        assert_eq!(params[4], "8", "Ctrl state bit set");
    }
}
