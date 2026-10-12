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

/// Encode a button release.
///
/// SGR (1006) reports the button that was released (`0` = left) with the
/// lowercase `m` suffix — this is what xterm and wezterm emit, and what
/// ConPTY (hence Windows console children like crossterm) expects. The
/// legacy X10/UTF-8 encodings cannot carry the button, so they use the
/// conventional button-3 release marker.
pub fn encode_release(
    button: MouseButton,
    col: u16,
    row: u16,
    mods: Mods,
    encoding: MouseEncoding,
) -> Vec<u8> {
    match encoding {
        MouseEncoding::Sgr => encode(base_button(button), true, false, col, row, mods, encoding),
        MouseEncoding::X10 | MouseEncoding::Utf8 => {
            encode(3, true, false, col, row, mods, encoding)
        }
    }
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
#[path = "test/mouse_tests.rs"]
mod tests;
