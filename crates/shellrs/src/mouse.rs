//! Mouse support: encode pointer events as xterm mouse reports for panes
//! whose app enabled tracking, and find URLs under the cursor.
//!
//! Apps opt in with DECSET 1000 (press), 1002 (+ release), 1003 (+ motion)
//! plus an encoding flag (1006 SGR, 1005 UTF-8, default X10); vt100 already
//! tracks the mode per screen, so this module only translates winit-level
//! events into report bytes and scans grid text for links.

use crate::keys::Mods;

/// Mouse buttons we forward (mapped from winit upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Report encoding negotiated by the app (DECSET 1006 / 1005 / default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEncoding {
    /// Classic single-byte `ESC [ M Cb Cx Cy` (DECSET 1000 family default).
    X10,
    /// Like X10 but coordinates as UTF-8 (DECSET 1005).
    Utf8,
    /// Decimal `ESC [ < Cb ; Cx ; Cy M/m` (DECSET 1006).
    Sgr,
}

/// What a pane wants reported, derived from its screen's mouse mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseState {
    /// Any tracking active (1000/1002/1003): forward presses (and wheel).
    pub tracking: bool,
    /// Report releases too (1002/1003).
    pub release: bool,
    /// Report motion without any button held (1003 any-event, not just drag).
    pub any_motion: bool,
    /// Report motion while a button is held (1003, both drag and any-event).
    pub drag_motion: bool,
    /// Byte encoding for the reports.
    pub encoding: MouseEncoding,
}

impl MouseState {
    /// Motion report wanted right now (`held` = left button is down).
    pub fn wants_motion(&self, held: bool) -> bool {
        self.tracking && (self.any_motion || (held && self.drag_motion))
    }
}

/// xterm button number before modifier/motion bits (wheel = 64/65).
fn base_button(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Modifier bits OR'd into the button code (xterm has no super bit).
fn modifier_bits(mods: Mods) -> u32 {
    let mut bits = 0;
    if mods.contains(Mods::SHIFT) {
        bits |= 4;
    }
    if mods.contains(Mods::ALT) {
        bits |= 8;
    }
    if mods.contains(Mods::CONTROL) {
        bits |= 16;
    }
    bits
}

/// Encode a button press (`ESC [ M ...` / `ESC [ < ... M`).
pub fn encode_press(
    button: MouseButton,
    col: u16,
    row: u16,
    mods: Mods,
    encoding: MouseEncoding,
) -> Vec<u8> {
    encode(base_button(button), false, false, col, row, mods, encoding)
}

/// Encode a button release (always button 3).
pub fn encode_release(col: u16, row: u16, mods: Mods, encoding: MouseEncoding) -> Vec<u8> {
    encode(3, true, false, col, row, mods, encoding)
}

/// Encode motion: `button` is the held button, or `None` for a free move
/// (any-event mode reports those with button 3).
pub fn encode_motion(
    button: Option<MouseButton>,
    col: u16,
    row: u16,
    mods: Mods,
    encoding: MouseEncoding,
) -> Vec<u8> {
    match button {
        Some(b) => encode(base_button(b), false, true, col, row, mods, encoding),
        None => encode(3, false, true, col, row, mods, encoding),
    }
}

/// Encode a wheel notch as buttons 64 (up) / 65 (down), press-only.
pub fn encode_wheel(up: bool, col: u16, row: u16, mods: Mods, encoding: MouseEncoding) -> Vec<u8> {
    encode(if up { 64 } else { 65 }, false, false, col, row, mods, encoding)
}

/// Shared report builder: `button` is the xterm code (3 = release/none),
/// `release` picks the `m` suffix (SGR), `motion` adds the 32 bit.
fn encode(
    button: u32,
    release: bool,
    motion: bool,
    col: u16,
    row: u16,
    mods: Mods,
    encoding: MouseEncoding,
) -> Vec<u8> {
    match encoding {
        MouseEncoding::Sgr => {
            let code = button + modifier_bits(mods) + if motion { 32 } else { 0 };
            // 1-based cells; SGR is decimal so no range clamp is needed.
            format!(
                "\x1b[<{code};{};{}{}",
                col as u32 + 1,
                row as u32 + 1,
                if release { 'm' } else { 'M' }
            )
            .into_bytes()
        }
        MouseEncoding::X10 | MouseEncoding::Utf8 => {
            // X10 has no modifier bits; motion adds 32 to the button code.
            let code = button + if motion { 32 } else { 0 };
            let mut out = vec![0x1b, b'[', b'M', (32 + code.min(255 - 32)) as u8];
            // Coordinates are 1-based; X10 clamps to one byte, UTF-8 encodes.
            let (cx, cy) = (col as u32 + 1, row as u32 + 1);
            match encoding {
                MouseEncoding::X10 => {
                    out.push(cx.min(223) as u8 + 32);
                    out.push(cy.min(223) as u8 + 32);
                }
                MouseEncoding::Utf8 => {
                    push_utf8_coord(&mut out, cx);
                    push_utf8_coord(&mut out, cy);
                }
                MouseEncoding::Sgr => unreachable!(),
            }
            out
        }
    }
}

/// Append a 1-based coordinate as a UTF-8 sequence (DECSET 1005).
fn push_utf8_coord(out: &mut Vec<u8>, coord: u32) {
    let c = char::from_u32(coord.max(1)).unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

/// True when `url` is safe to hand to the OS opener: `http(s)` only, with
/// something after the scheme. `find_url` already normalizes to this shape;
/// this is the last line of defense before `open::that` (which never uses
/// a shell, so metacharacters cannot escape anyway).
pub fn is_openable_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    match rest {
        Some(r) => !r.trim().is_empty() && !r.contains([' ', '\t', '\n', '<', '>', '"']),
        None => false,
    }
}

/// Find the URL under `(row, col)` in padded grid lines. Matches `http(s)://`
/// and `www.` runs (the latter gains an `https://` prefix), strips trailing
/// `.,;:!?'"` and unbalanced closers. Columns are char indices, matching
/// how the grid snapshot is padded.
pub fn find_url(lines: &[String], row: usize, col: usize) -> Option<String> {
    let line: Vec<char> = lines.get(row)?.chars().collect();
    if col >= line.len() {
        return None;
    }
    // Candidate starts: scheme or www. markers at or before the cursor.
    let text: String = line.iter().collect();
    let mut best: Option<(usize, usize)> = None;
    for marker in ["https://", "http://", "www."] {
        let mut search_from = 0;
        while let Some(rel) = text[search_from..].find(marker) {
            let start = search_from + rel;
            let start_char = text[..start].chars().count();
            let end_char = match_end(&line, start_char + marker.chars().count());
            if start_char <= col && col < end_char {
                let span = (start_char, end_char);
                // Longest span containing the cursor wins (a `www.` inside
                // an `https://` URL belongs to the full match).
                if best.map(|(s, e)| span.1 - span.0 > e - s).unwrap_or(true) {
                    best = Some(span);
                }
            }
            search_from = start + marker.len().max(1);
            if search_from >= text.len() {
                break;
            }
        }
    }
    let (start, end) = best?;
    if start >= end {
        return None;
    }
    let raw: String = line[start..end].iter().collect();
    let trimmed = trim_url(&raw);
    // Reject bare markers with nothing after them (`https://`, `www.`).
    let min_len = if trimmed.starts_with("www.") {
        5
    } else if trimmed.starts_with("https://") {
        9
    } else if trimmed.starts_with("http://") {
        8
    } else {
        return None;
    };
    if trimmed.len() < min_len {
        return None;
    }
    if trimmed.starts_with("www.") {
        Some(format!("https://{trimmed}"))
    } else {
        Some(trimmed.to_string())
    }
}

/// End (char index, exclusive) of the URL starting after `marker_end`:
/// first whitespace, quote, angle bracket or backtick.
fn match_end(line: &[char], marker_end: usize) -> usize {
    let mut end = marker_end;
    while end < line.len() {
        let c = line[end];
        if c.is_whitespace() || matches!(c, '"' | '<' | '>' | '`' | '|') {
            break;
        }
        end += 1;
    }
    end
}

/// Strip trailing `.,;:!?'"` always, then unbalanced `)`/`]` closers
/// (balanced ones, e.g. Wikipedia paths, are kept).
fn trim_url(raw: &str) -> &str {
    let mut s = raw.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"']);
    loop {
        let (open, close) = match s.chars().last() {
            Some(')') => ('(', ')'),
            Some(']') => ('[', ']'),
            _ => break,
        };
        let opens = s.chars().filter(|&c| c == open).count();
        let closes = s.chars().filter(|&c| c == close).count();
        if closes > opens {
            s = &s[..s.len() - close.len_utf8()];
        } else {
            break;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sgr(button: u32, release: bool, col: u16, row: u16) -> Vec<u8> {
        encode(button, release, false, col, row, Mods::empty(), MouseEncoding::Sgr)
    }

    #[test]
    fn sgr_press_release_motion_vectors() {
        // Left press at (0,0) -> 1-based (1,1).
        assert_eq!(sgr(0, false, 0, 0), b"\x1b[<0;1;1M".to_vec());
        assert_eq!(
            encode_press(MouseButton::Left, 4, 9, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<0;5;10M".to_vec()
        );
        // Release is always button 3 with `m`.
        assert_eq!(
            encode_release(4, 9, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<3;5;10m".to_vec()
        );
        // Drag adds the motion bit (0 + 32).
        assert_eq!(
            encode_motion(
                Some(MouseButton::Left),
                4,
                9,
                Mods::empty(),
                MouseEncoding::Sgr
            ),
            b"\x1b[<32;5;10M".to_vec()
        );
        // Free move reports button 3 with `M`.
        assert_eq!(
            encode_motion(None, 4, 9, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<35;5;10M".to_vec()
        );
        // Wheel up/down are buttons 64/65, press-only.
        assert_eq!(
            encode_wheel(true, 0, 0, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<64;1;1M".to_vec()
        );
        assert_eq!(
            encode_wheel(false, 79, 24, Mods::empty(), MouseEncoding::Sgr),
            b"\x1b[<65;80;25M".to_vec()
        );
        // Modifiers OR into the button code: ctrl(16) + left drag (0+32).
        let mut mods = Mods::empty();
        mods.insert(Mods::CONTROL);
        assert_eq!(
            encode_motion(Some(MouseButton::Left), 0, 0, mods, MouseEncoding::Sgr),
            b"\x1b[<48;1;1M".to_vec()
        );
        // Middle and right buttons.
        assert_eq!(sgr(1, false, 0, 0), b"\x1b[<1;1;1M".to_vec());
        assert_eq!(sgr(2, false, 0, 0), b"\x1b[<2;1;1M".to_vec());
    }

    #[test]
    fn x10_and_utf8_vectors() {
        // Left press at (4,9): ESC M, 32+0, 32+5, 32+10.
        assert_eq!(
            encode_press(MouseButton::Left, 4, 9, Mods::empty(), MouseEncoding::X10),
            vec![0x1b, b'[', b'M', 32, 37, 42]
        );
        // Release: button 3.
        assert_eq!(
            encode_release(4, 9, Mods::empty(), MouseEncoding::X10),
            vec![0x1b, b'[', b'M', 35, 37, 42]
        );
        // X10 has no modifier bits: shift is ignored.
        let mut mods = Mods::empty();
        mods.insert(Mods::SHIFT);
        assert_eq!(
            encode_press(MouseButton::Left, 0, 0, mods, MouseEncoding::X10),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        // X10 clamps coordinates to one byte.
        assert_eq!(
            encode_press(MouseButton::Left, 500, 500, Mods::empty(), MouseEncoding::X10),
            vec![0x1b, b'[', b'M', 32, 255, 255]
        );
        // UTF-8 encodes coordinates as raw codepoints, not ASCII digits:
        // col 4 -> 5 (one byte 0x05), row 9 -> 10 (one byte 0x0A).
        assert_eq!(
            encode_press(MouseButton::Left, 4, 9, Mods::empty(), MouseEncoding::Utf8),
            vec![0x1b, b'[', b'M', 32, 5, 10]
        );
        // Large coordinates become multibyte sequences (301 -> U+012D).
        assert_eq!(
            encode_press(MouseButton::Left, 300, 9, Mods::empty(), MouseEncoding::Utf8),
            vec![0x1b, b'[', b'M', 32, 0xC4, 0xAD, 10]
        );
        // Wheel in X10: 32+64 / 32+65.
        assert_eq!(
            encode_wheel(true, 0, 0, Mods::empty(), MouseEncoding::X10),
            vec![0x1b, b'[', b'M', 96, 33, 33]
        );
    }

    #[test]
    fn url_middle_and_edges() {
        let lines = vec!["see https://example.com/a?b=1 here".to_string()];
        assert_eq!(
            find_url(&lines, 0, 10),
            Some("https://example.com/a?b=1".to_string())
        );
        // First and last char of the URL.
        assert_eq!(
            find_url(&lines, 0, 4),
            Some("https://example.com/a?b=1".to_string())
        );
        assert_eq!(
            find_url(&lines, 0, 28),
            Some("https://example.com/a?b=1".to_string())
        );
        // Just outside either end: nothing.
        assert_eq!(find_url(&lines, 0, 3), None);
        assert_eq!(find_url(&lines, 0, 29), None);
        // No URL on this row / out of range.
        assert_eq!(find_url(&["plain text".to_string()], 0, 2), None);
        assert_eq!(find_url(&lines, 3, 0), None);
        assert_eq!(find_url(&lines, 0, 500), None);
    }

    #[test]
    fn url_trims_punctuation_and_balances_parens() {
        // Trailing sentence punctuation is not part of the link.
        let lines = vec!["go https://example.com/x., next".to_string()];
        assert_eq!(
            find_url(&lines, 0, 5),
            Some("https://example.com/x".to_string())
        );
        // Balanced parens (Wikipedia style) are kept...
        let lines = vec!["a https://en.wikipedia.org/wiki/X_(thing) ok".to_string()];
        assert_eq!(
            find_url(&lines, 0, 30),
            Some("https://en.wikipedia.org/wiki/X_(thing)".to_string())
        );
        // ...while an extra closer from surrounding prose is dropped.
        let lines = vec!["(see https://example.com/y))".to_string()];
        assert_eq!(
            find_url(&lines, 0, 10),
            Some("https://example.com/y".to_string())
        );
        // www. gains a scheme.
        let lines = vec!["open www.example.org/a!".to_string()];
        assert_eq!(
            find_url(&lines, 0, 7),
            Some("https://www.example.org/a".to_string())
        );
        // Bare markers with nothing after them are not links.
        let lines = vec!["go https:// next".to_string()];
        assert_eq!(find_url(&lines, 0, 5), None);
        let lines = vec!["visit www. now".to_string()];
        assert_eq!(find_url(&lines, 0, 7), None);
    }

    #[test]
    fn openable_rejects_non_http() {
        assert!(is_openable_url("https://example.com"));
        assert!(is_openable_url("http://localhost:3000/x"));
        assert!(!is_openable_url(""));
        assert!(!is_openable_url("https://"));
        assert!(!is_openable_url("file:///etc/passwd"));
        assert!(!is_openable_url("ftp://example.com"));
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url("www.example.com"));
        assert!(!is_openable_url("https://exa mple.com"));
    }
}
