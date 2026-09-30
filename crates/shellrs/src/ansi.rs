//! Plain-text extraction from terminal byte streams.
//!
//! Used for yanking: strips ECMA-48 escape sequences (CSI/OSC/charset/single)
//! and normalizes line endings, so copied text contains no control garbage.
//! It also records OSC 133 prompt markers, which shell integration emits,
//! so callers can find where a command's output starts and ends.

/// Strip escape sequences from terminal output; `\r\n` and lone `\r`
/// become `\n`, other control characters are dropped.
pub fn strip_ansi_markers(input: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(input.len());
    let mut markers = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if skip_escape(&mut chars) {
                    markers.push(out.len());
                }
            }
            '\r' => {
                // Fold CRLF (and lone CR) into a single newline.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    (out, markers)
}

/// All `file://` directory URIs reported via OSC 7 in `input`
/// (`ESC ] 7 ; file://host/path ST`, BEL- or `ESC \` terminated).
/// Percent-decoding is applied and a leading `/` is stripped from
/// Windows paths (`/C:/foo` -> `C:/foo`). Malformed entries are skipped;
/// callers decide whether the path must exist.
pub fn extract_osc7_dirs(input: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        // Look for `ESC ] 7 ;`.
        if bytes[i] == 0x1b && bytes[i + 1] == b']' && bytes[i + 2] == b'7' && bytes[i + 3] == b';'
        {
            let start = i + 4;
            // Payload ends at BEL or `ESC \` (ST).
            let mut end = None;
            let mut j = start;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    end = Some((j, j + 1));
                    break;
                }
                if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                    end = Some((j, j + 2));
                    break;
                }
                j += 1;
            }
            match end {
                Some((stop, next)) => {
                    if let Ok(payload) = std::str::from_utf8(&bytes[start..stop]) {
                        if let Some(p) = osc7_uri_to_path(payload.trim()) {
                            out.push(p);
                        }
                    }
                    i = next;
                }
                // Unterminated (split across reads): stop, the caller
                // retries with a tail prepended on the next chunk.
                None => break,
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Turn an OSC 7 payload (`file://host/path`, percent-encoded) into a path.
/// Remote hosts are kept as-is (callers filter with `is_dir`); a lone path
/// without the `file://` scheme is accepted too for lenient emitters.
fn osc7_uri_to_path(payload: &str) -> Option<std::path::PathBuf> {
    let rest = payload.strip_prefix("file://").unwrap_or(payload);
    // Split `host/path`: the first `/` ends the host part.
    let path = match rest.find('/') {
        Some(i) => &rest[i..],
        // No `/` at all: a bare host with no path is useless.
        None => return None,
    };
    let decoded = percent_decode(path);
    if decoded.is_empty() {
        return None;
    }
    // Windows drive URIs arrive as `/C:/...`; drop that leading slash.
    let bytes = decoded.as_bytes();
    let fixed = if decoded.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        decoded[1..].to_string()
    } else {
        decoded
    };
    Some(std::path::PathBuf::from(fixed))
}

/// Decode `%XX` sequences; lone/invalid `%` is kept literally.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Consume one escape sequence starting after `ESC`. Returns true when the
/// sequence was a prompt-start marker (OSC `133;A`).
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars>) -> bool {
    match chars.peek() {
        // CSI: ESC [ params intermediates final.
        Some('[') => {
            chars.next();
            for c in chars.by_ref() {
                if matches!(c, '@'..='~') {
                    break;
                }
            }
            false
        }
        // OSC: ESC ] payload terminated by BEL or ESC \.
        Some(']') => {
            chars.next();
            let mut payload = String::new();
            loop {
                match chars.next() {
                    None => break,
                    Some('\x07') => break,
                    Some('\x1b') => {
                        // ST is ESC \; swallow the trailing backslash.
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                    Some(c) => payload.push(c),
                }
            }
            payload == "133;A"
        }
        // Charset / misc 3-byte sequences: ESC ( X, ESC ) X, ESC # X.
        Some('(') | Some(')') | Some('#') => {
            chars.next();
            chars.next();
            false
        }
        // Any other single character after ESC.
        Some(_) => {
            chars.next();
            false
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        strip_ansi_markers(s).0
    }

    #[test]
    fn strips_sgr_and_cursor_sequences() {
        let raw = "\x1b[1;32mhello\x1b[0m world\x1b[K\r\n\x1b[2Kprompt> ";
        assert_eq!(strip_ansi(raw), "hello world\nprompt> ");
    }

    #[test]
    fn strips_osc_hyperlinks_and_titles() {
        // OSC payloads (titles, hyperlinks) are dropped entirely.
        let raw = "\x1b]0;title\x07text\x1b]8;;http://x\x07link\x1b]8;;\x07";
        assert_eq!(strip_ansi(raw), "textlink");
    }

    #[test]
    fn lone_cr_becomes_newline() {
        assert_eq!(strip_ansi("a\rb"), "a\nb");
    }

    #[test]
    fn drops_other_controls_keeps_tabs() {
        assert_eq!(strip_ansi("a\x00b\tc\x07"), "ab\tc");
    }

    #[test]
    fn records_prompt_marker_offsets() {
        // ESC ] 133;A ST, then a prompt line.
        let raw = "out\x1b]133;A\x1b\\PS> ";
        let (text, marks) = strip_ansi_markers(raw);
        assert_eq!(text, "outPS> ");
        assert_eq!(marks, vec![3]);
    }

    #[test]
    fn osc7_bel_and_st_terminators() {
        let bel = "hi\x1b]7;file://host/C:/Users/me\x07there";
        let st = "hi\x1b]7;file:///home/user\x1b\\there";
        let got: Vec<String> = extract_osc7_dirs(bel)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, vec!["C:/Users/me"]);
        let got: Vec<String> = extract_osc7_dirs(st)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        // Unix path keeps its leading slash.
        assert_eq!(got, vec!["/home/user"]);
    }

    #[test]
    fn osc7_percent_decoding_and_skips_junk() {
        let raw = "\x1b]7;file://h/C:/My%20Docs%2Fa\x07\x1b]0;title\x07\x1b]7;notauri\x07";
        let got: Vec<String> = extract_osc7_dirs(raw)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, vec!["C:/My Docs/a"]);
        // Unterminated tail yields nothing (caller retries with more bytes).
        assert!(extract_osc7_dirs("\x1b]7;file://h/C:/half").is_empty());
        // Bad `%` survives literally, empty path is dropped.
        assert_eq!(percent_decode("a%2Fb%zzc%"), "a/b%zzc%");
        assert!(osc7_uri_to_path("file://h").is_none());
    }
}
