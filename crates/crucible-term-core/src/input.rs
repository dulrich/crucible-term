//! Pure keyboard and mouse encoders: CDE-owned input types in, terminal bytes out. rio-vt
//! 0.5.29 has no key encoder (it only tracks the keyboard and mouse modes), so the encodings
//! live here. The window translates xkb/sctk events into these types; nothing here names a
//! Wayland, xkbcommon or rio-vt type.
//!
//! Key encoding covers the legacy xterm forms (text, Ctrl+letter C0, Alt as an ESC prefix,
//! cursor keys honouring DECCKM, `ESC [ n ~` editing keys, F1-F12, `1;m` modifier parameters)
//! and the kitty keyboard protocol flags `0b1` (disambiguate) and `0b1000` (report all keys as
//! escape codes), as `CSI unicode-key-code ; modifiers u`. Kitty flags `0b10` (event types),
//! `0b100` (alternate keys) and `0b10000` (associated text) are accepted but encoded as if
//! unset: only key presses (and repeats) are reported, with the produced character lowercased
//! as the key code and no shifted/base-layout alternates. Application keypad (DECKPAM) is
//! tracked in [`TermModes`] but not encoded: keypad keys arrive as ordinary characters.
//!
//! Mouse encoding covers press, release, motion and wheel for modes 1000/1002/1003 in the
//! default X10-style and SGR (1006) encodings. Encodings 1005 (UTF-8) and 1015 (urxvt) are
//! ignored: a child that sets them is answered in the default encoding.

/// Modifier keys held during an input event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    /// Shift.
    pub shift: bool,
    /// Alt.
    pub alt: bool,
    /// Control.
    pub ctrl: bool,
    /// Super / logo.
    pub logo: bool,
}

impl Mods {
    /// The xterm / kitty modifier parameter minus one (shift 1, alt 2, ctrl 4, super 8).
    fn bits(self) -> u32 {
        u32::from(self.shift)
            | u32::from(self.alt) << 1
            | u32::from(self.ctrl) << 2
            | u32::from(self.logo) << 3
    }

    /// True when no modifier is held.
    pub fn none(self) -> bool {
        self.bits() == 0
    }
}

/// A key as the window decodes it from the compositor's keymap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A text-producing key: the character it produces (shift applied) without Ctrl/Alt effects.
    Char(char),
    /// Return / Enter.
    Enter,
    /// Tab (Shift+Tab is `Tab` with `shift`).
    Tab,
    /// Backspace.
    Backspace,
    /// Escape.
    Escape,
    /// Cursor up.
    Up,
    /// Cursor down.
    Down,
    /// Cursor right.
    Right,
    /// Cursor left.
    Left,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Insert.
    Insert,
    /// Delete.
    Delete,
    /// Function key F1-F12 (other numbers encode as nothing).
    F(u8),
}

/// Which mouse events the child asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    /// No reporting.
    #[default]
    Off,
    /// 1000 (and the X10 mode 9, which is folded in): button press and release.
    Click,
    /// 1002: plus motion while a button is held.
    Drag,
    /// 1003: plus all motion.
    Any,
}

/// How mouse events are encoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseEncoding {
    /// `CSI M` with three bytes offset by 32 (coordinates clamp at 223).
    #[default]
    Default,
    /// 1006: `CSI < b ; x ; y M|m`.
    Sgr,
}

/// The terminal modes input depends on, read from [`crate::Session::modes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermModes {
    /// DECCKM (1): cursor keys send `ESC O x`.
    pub app_cursor: bool,
    /// DECKPAM (`ESC =`): application keypad (tracked; not encoded, see the module docs).
    pub app_keypad: bool,
    /// Kitty keyboard flags at the top of the stack (`0b1` disambiguate, `0b10` event types,
    /// `0b100` alternate keys, `0b1000` all keys as escape codes, `0b10000` associated text).
    pub kitty_flags: u8,
    /// Requested mouse reporting.
    pub mouse: MouseMode,
    /// Mouse encoding (default or SGR 1006).
    pub mouse_encoding: MouseEncoding,
    /// 2004: bracketed paste.
    pub bracketed_paste: bool,
    /// 1004: focus in/out reports.
    pub focus_events: bool,
    /// The alternate screen is active.
    pub alt_screen: bool,
    /// 1007: alternate scroll (wheel sends arrow keys on the alternate screen).
    pub alternate_scroll: bool,
}

const KITTY_DISAMBIGUATE: u8 = 0b1;
const KITTY_ALL_KEYS: u8 = 0b1000;

/// Encode a key press (or repeat). `None` means send nothing (unencodable key).
pub fn encode_key(key: Key, mods: Mods, modes: &TermModes) -> Option<Vec<u8>> {
    let all = modes.kitty_flags & KITTY_ALL_KEYS != 0;
    let disambiguate = modes.kitty_flags & (KITTY_DISAMBIGUATE | KITTY_ALL_KEYS) != 0;
    let m = mods.bits();
    let csi_u = |code: u32| -> Vec<u8> {
        if m == 0 {
            format!("\x1b[{code}u").into_bytes()
        } else {
            format!("\x1b[{code};{}u", m + 1).into_bytes()
        }
    };
    match key {
        Key::Char(c) => {
            if c.is_control() {
                return None;
            }
            let code = || c.to_lowercase().next().map_or(c as u32, |l| l as u32);
            if all {
                return Some(csi_u(code()));
            }
            if disambiguate && (mods.ctrl || mods.alt || mods.logo) {
                return Some(csi_u(code()));
            }
            Some(legacy_char(c, mods))
        }
        Key::Enter | Key::Tab | Key::Backspace | Key::Escape => {
            let code = match key {
                Key::Enter => 13,
                Key::Tab => 9,
                Key::Backspace => 127,
                _ => 27,
            };
            // Escape is always disambiguated; the others only with all-keys or a modifier
            // (Shift+Tab alone stays `CSI Z` in legacy terms).
            let tab_shift_only = key == Key::Tab && mods.shift && m == 1;
            if all
                || (disambiguate && key == Key::Escape)
                || (disambiguate && m != 0 && !tab_shift_only)
            {
                return Some(csi_u(code));
            }
            Some(legacy_special(key, mods))
        }
        Key::Up | Key::Down | Key::Right | Key::Left | Key::Home | Key::End => {
            let ch = match key {
                Key::Up => b'A',
                Key::Down => b'B',
                Key::Right => b'C',
                Key::Left => b'D',
                Key::Home => b'H',
                _ => b'F',
            };
            Some(if m != 0 {
                format!("\x1b[1;{}{}", m + 1, ch as char).into_bytes()
            } else if modes.app_cursor {
                vec![0x1b, b'O', ch]
            } else {
                vec![0x1b, b'[', ch]
            })
        }
        Key::PageUp => Some(tilde(5, m)),
        Key::PageDown => Some(tilde(6, m)),
        Key::Insert => Some(tilde(2, m)),
        Key::Delete => Some(tilde(3, m)),
        Key::F(n) => match n {
            1..=4 => {
                let ch = b"PQRS"[usize::from(n - 1)] as char;
                Some(if m == 0 {
                    format!("\x1bO{ch}").into_bytes()
                } else {
                    format!("\x1b[1;{}{ch}", m + 1).into_bytes()
                })
            }
            5..=12 => {
                let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)];
                Some(tilde(code, m))
            }
            _ => None,
        },
    }
}

/// `ESC [ n ~`, or `ESC [ n ; m ~` with modifiers (`m` is the bit mask, parameter is `m + 1`).
fn tilde(n: u32, m: u32) -> Vec<u8> {
    if m == 0 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{}~", m + 1).into_bytes()
    }
}

fn legacy_char(c: char, mods: Mods) -> Vec<u8> {
    let mut out = Vec::new();
    if mods.alt {
        out.push(0x1b);
    }
    if mods.ctrl {
        let ctrl = match c {
            ' ' | '@' | '2' => Some(0x00),
            'a'..='z' => Some(c as u8 - b'a' + 1),
            'A'..='Z' => Some(c as u8 - b'A' + 1),
            '[' | '3' => Some(0x1b),
            '\\' | '4' => Some(0x1c),
            ']' | '5' => Some(0x1d),
            '^' | '6' => Some(0x1e),
            '_' | '7' | '/' => Some(0x1f),
            '?' | '8' => Some(0x7f),
            _ => None,
        };
        if let Some(b) = ctrl {
            out.push(b);
            return out;
        }
    }
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    out
}

fn legacy_special(key: Key, mods: Mods) -> Vec<u8> {
    let mut out = Vec::new();
    if mods.alt {
        out.push(0x1b);
    }
    match key {
        Key::Enter => out.push(b'\r'),
        Key::Tab if mods.shift => out.extend_from_slice(b"\x1b[Z"),
        Key::Tab => out.push(b'\t'),
        Key::Backspace => out.push(if mods.ctrl { 0x08 } else { 0x7f }),
        _ => out.push(0x1b),
    }
    out
}

/// A mouse button, or the wheel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    /// Left button.
    Left,
    /// Middle button.
    Middle,
    /// Right button.
    Right,
    /// Wheel up (away from the user).
    WheelUp,
    /// Wheel down.
    WheelDown,
    /// No button (motion only).
    None,
}

/// What happened to the button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    /// Pressed (wheel notches are presses).
    Press,
    /// Released.
    Release,
    /// Moved.
    Motion,
}

/// One mouse event at a 0-based grid cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    /// Press, release or motion.
    pub action: MouseAction,
    /// Which button (for motion: the held button, or `None`).
    pub button: MouseButton,
    /// Held modifiers.
    pub mods: Mods,
    /// Column, 0-based.
    pub col: usize,
    /// Row, 0-based.
    pub row: usize,
}

/// Encode a mouse event under the child's requested mode. `None` means the mode does not report
/// this event (or reporting is off); never emits an invalid byte.
pub fn encode_mouse(ev: MouseEvent, modes: &TermModes) -> Option<Vec<u8>> {
    let wheel = matches!(ev.button, MouseButton::WheelUp | MouseButton::WheelDown);
    match (modes.mouse, ev.action) {
        (MouseMode::Off, _) => return None,
        (MouseMode::Click, MouseAction::Motion) => return None,
        (MouseMode::Drag, MouseAction::Motion) if ev.button == MouseButton::None => return None,
        (_, MouseAction::Release) if wheel => return None,
        _ => {}
    }
    let mut b = match ev.button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::None => 3,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    let sgr = modes.mouse_encoding == MouseEncoding::Sgr;
    if ev.action == MouseAction::Release && !sgr {
        b = 3;
    }
    if ev.action == MouseAction::Motion {
        b += 32;
    }
    b += u32::from(ev.mods.shift) * 4 + u32::from(ev.mods.alt) * 8 + u32::from(ev.mods.ctrl) * 16;
    if sgr {
        let end = if ev.action == MouseAction::Release {
            'm'
        } else {
            'M'
        };
        Some(format!("\x1b[<{b};{};{}{end}", ev.col + 1, ev.row + 1).into_bytes())
    } else {
        // Coordinates are 1-based plus 32 in one byte: clamp so the byte stays <= 255.
        let enc = |v: usize| (v.min(222) + 1 + 32) as u8;
        Some(vec![
            0x1b,
            b'[',
            b'M',
            (b + 32) as u8,
            enc(ev.col),
            enc(ev.row),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        shift: false,
        alt: false,
        ctrl: false,
        logo: false,
    };
    const CTRL: Mods = Mods { ctrl: true, ..NONE };
    const ALT: Mods = Mods { alt: true, ..NONE };
    const SHIFT: Mods = Mods {
        shift: true,
        ..NONE
    };

    fn modes(kitty: u8, app_cursor: bool) -> TermModes {
        TermModes {
            kitty_flags: kitty,
            app_cursor,
            ..TermModes::default()
        }
    }

    fn check(m: &TermModes, rows: &[(Key, Mods, Option<&[u8]>)]) {
        for (k, mods, want) in rows {
            assert_eq!(encode_key(*k, *mods, m).as_deref(), *want, "{k:?} {mods:?}");
        }
    }

    #[test]
    fn legacy_keys() {
        check(
            &modes(0, false),
            &[
                (Key::Char('a'), NONE, Some(b"a")),
                (Key::Char('é'), NONE, Some("é".as_bytes())),
                (Key::Char('a'), ALT, Some(b"\x1ba")),
                (Key::Char('c'), CTRL, Some(b"\x03")),
                (Key::Char('C'), CTRL, Some(b"\x03")),
                (Key::Char(' '), CTRL, Some(b"\x00")),
                (Key::Char('['), CTRL, Some(b"\x1b")),
                (Key::Enter, NONE, Some(b"\r")),
                (Key::Tab, NONE, Some(b"\t")),
                (Key::Tab, SHIFT, Some(b"\x1b[Z")),
                (Key::Backspace, NONE, Some(b"\x7f")),
                (Key::Backspace, CTRL, Some(b"\x08")),
                (Key::Escape, NONE, Some(b"\x1b")),
                (Key::PageUp, NONE, Some(b"\x1b[5~")),
                (Key::PageDown, NONE, Some(b"\x1b[6~")),
                (Key::Insert, NONE, Some(b"\x1b[2~")),
                (Key::Delete, NONE, Some(b"\x1b[3~")),
                (Key::F(1), NONE, Some(b"\x1bOP")),
                (Key::F(5), NONE, Some(b"\x1b[15~")),
                (Key::F(12), NONE, Some(b"\x1b[24~")),
                (Key::F(13), NONE, None),
                (Key::Char('\u{7}'), NONE, None),
            ],
        );
    }

    #[test]
    fn decckm_switches_cursor_keys() {
        check(
            &modes(0, false),
            &[
                (Key::Up, NONE, Some(b"\x1b[A")),
                (Key::Home, NONE, Some(b"\x1b[H")),
            ],
        );
        check(
            &modes(0, true),
            &[
                (Key::Up, NONE, Some(b"\x1bOA")),
                (Key::Left, NONE, Some(b"\x1bOD")),
                (Key::End, NONE, Some(b"\x1bOF")),
                // Modifiers always use the CSI form.
                (Key::Up, CTRL, Some(b"\x1b[1;5A")),
            ],
        );
    }

    #[test]
    fn modifier_parameters() {
        let all = Mods {
            shift: true,
            alt: true,
            ctrl: true,
            logo: true,
        };
        check(
            &modes(0, false),
            &[
                (Key::Right, SHIFT, Some(b"\x1b[1;2C")),
                (Key::Left, ALT, Some(b"\x1b[1;3D")),
                (Key::Down, all, Some(b"\x1b[1;16B")),
                (Key::Delete, CTRL, Some(b"\x1b[3;5~")),
                (Key::F(2), SHIFT, Some(b"\x1b[1;2Q")),
                (Key::F(6), CTRL, Some(b"\x1b[17;5~")),
            ],
        );
    }

    #[test]
    fn kitty_disambiguate() {
        check(
            &modes(1, false),
            &[
                (Key::Escape, NONE, Some(b"\x1b[27u")),
                (Key::Char('a'), NONE, Some(b"a")),
                (Key::Char('a'), SHIFT, Some(b"a")),
                (Key::Char('i'), CTRL, Some(b"\x1b[105;5u")),
                (
                    Key::Char('A'),
                    Mods {
                        ctrl: true,
                        shift: true,
                        ..NONE
                    },
                    Some(b"\x1b[97;6u"),
                ),
                (Key::Char('x'), ALT, Some(b"\x1b[120;3u")),
                (Key::Enter, NONE, Some(b"\r")),
                (Key::Enter, SHIFT, Some(b"\x1b[13;2u")),
                (Key::Tab, SHIFT, Some(b"\x1b[Z")),
                (Key::Backspace, CTRL, Some(b"\x1b[127;5u")),
                (Key::Up, NONE, Some(b"\x1b[A")),
                // Out-of-scope flags 0b10/0b100/0b10000 encode as if unset.
                (Key::Char('a'), NONE, Some(b"a")),
            ],
        );
        assert_eq!(
            encode_key(Key::Escape, NONE, &modes(0b10111, false)).as_deref(),
            Some(&b"\x1b[27u"[..])
        );
    }

    #[test]
    fn kitty_all_keys() {
        check(
            &modes(0b1001, false),
            &[
                (Key::Char('a'), NONE, Some(b"\x1b[97u")),
                (Key::Char('A'), SHIFT, Some(b"\x1b[97;2u")),
                (Key::Char('é'), NONE, Some("\x1b[233u".as_bytes())),
                (Key::Enter, NONE, Some(b"\x1b[13u")),
                (Key::Tab, NONE, Some(b"\x1b[9u")),
                (Key::Backspace, NONE, Some(b"\x1b[127u")),
                (Key::Left, NONE, Some(b"\x1b[D")),
                (Key::F(3), NONE, Some(b"\x1bOR")),
            ],
        );
    }

    fn mouse(action: MouseAction, button: MouseButton, col: usize, row: usize) -> MouseEvent {
        MouseEvent {
            action,
            button,
            mods: NONE,
            col,
            row,
        }
    }

    fn mm(mouse: MouseMode, enc: MouseEncoding) -> TermModes {
        TermModes {
            mouse,
            mouse_encoding: enc,
            ..TermModes::default()
        }
    }

    #[test]
    fn mouse_sgr_and_gating() {
        use MouseAction::*;
        use MouseButton::*;
        let sgr = |m| mm(m, MouseEncoding::Sgr);
        let e = |m: &TermModes, ev| encode_mouse(ev, m);
        assert_eq!(
            e(&sgr(MouseMode::Off), mouse(Press, Left, 0, 0)),
            Option::None
        );
        let click = sgr(MouseMode::Click);
        assert_eq!(
            e(&click, mouse(Press, Left, 4, 9)).as_deref(),
            Some(&b"\x1b[<0;5;10M"[..])
        );
        assert_eq!(
            e(&click, mouse(Release, Left, 4, 9)).as_deref(),
            Some(&b"\x1b[<0;5;10m"[..])
        );
        assert_eq!(
            e(&click, mouse(Press, WheelUp, 0, 0)).as_deref(),
            Some(&b"\x1b[<64;1;1M"[..])
        );
        assert_eq!(e(&click, mouse(Release, WheelUp, 0, 0)), Option::None);
        assert_eq!(e(&click, mouse(Motion, Left, 1, 1)), Option::None);
        let drag = sgr(MouseMode::Drag);
        assert_eq!(
            e(&drag, mouse(Motion, Left, 1, 1)).as_deref(),
            Some(&b"\x1b[<32;2;2M"[..])
        );
        assert_eq!(
            e(&drag, mouse(Motion, MouseButton::None, 1, 1)),
            Option::None
        );
        let any = sgr(MouseMode::Any);
        assert_eq!(
            e(&any, mouse(Motion, MouseButton::None, 1, 1)).as_deref(),
            Some(&b"\x1b[<35;2;2M"[..])
        );
        let ev = MouseEvent {
            mods: CTRL,
            ..mouse(Press, Right, 0, 0)
        };
        assert_eq!(e(&any, ev).as_deref(), Some(&b"\x1b[<18;1;1M"[..]));
    }

    #[test]
    fn mouse_default_encoding_clamps() {
        use MouseAction::*;
        use MouseButton::*;
        let m = mm(MouseMode::Any, MouseEncoding::Default);
        assert_eq!(
            encode_mouse(mouse(Press, Left, 0, 0), &m).as_deref(),
            Some(&[0x1b, b'[', b'M', 32, 33, 33][..])
        );
        // Release reports button 3.
        assert_eq!(
            encode_mouse(mouse(Release, Left, 1, 2), &m).as_deref(),
            Some(&[0x1b, b'[', b'M', 35, 34, 35][..])
        );
        // Far outside the 223-column range: clamped to the last encodable byte, never > 255.
        let out = encode_mouse(mouse(Press, Right, 5000, 300), &m).expect("bytes");
        assert_eq!(out, vec![0x1b, b'[', b'M', 34, 255, 255]);
    }
}
